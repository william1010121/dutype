//! Snapshot-cursor behavior through the public, line-delimited MCP protocol.
//! Interactive requests let the fixture change only after a completed scan.

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!(
            "diskbytype-mcp-pagination-{}-{nonce}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("tree");
        fs::create_dir_all(&root).unwrap();
        let config = base.join("config.toml");
        fs::write(&config, "").unwrap();
        Self { base, root, config }
    }

    fn command(&self, threads: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_diskbytype"));
        command
            .arg("--config")
            .arg(&self.config)
            .current_dir(&self.base)
            .env("DUTYPE_CACHE_DIR", self.base.join("cache"))
            .env("RAYON_NUM_THREADS", threads);
        command
    }

    fn server(&self, options: &[&str], threads: &str) -> Server {
        let stderr = self.base.join(format!(
            "server-{}.stderr",
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let child = self
            .command(threads)
            .arg("--mcp")
            .args(options)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        Server::new(child, stderr)
    }

    fn cli(&self, root: &Path, category: Option<&str>, top: usize, options: &[&str], threads: &str) -> Value {
        let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let stdout = self.base.join(format!("cli-{suffix}.json"));
        let stderr = self.base.join(format!("cli-{suffix}.stderr"));
        let mut command = self.command(threads);
        command.arg("--json").arg("--top").arg(top.to_string());
        if let Some(category) = category {
            command.args(["--category", category]);
        }
        let mut child = command
            .args(options)
            .arg(root)
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let status = wait_for_exit(&mut child);
        let errors = fs::read_to_string(stderr).unwrap();
        assert!(status.success(), "CLI failed: {errors}");
        assert!(errors.is_empty(), "{errors}");
        serde_json::from_slice(&fs::read(stdout).unwrap()).unwrap()
    }

    fn pdfs(&self, count: usize) {
        for i in 0..count {
            fs::write(self.root.join(format!("{i:04}.pdf")), b"").unwrap();
        }
    }

    fn no_disk_cache(&self) {
        assert!(!self.base.join("cache").exists(), "MCP must not create a disk cache");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("diskbytype subprocess timed out");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    stderr: PathBuf,
    next_id: u64,
}

impl Server {
    fn new(mut child: Child, stderr: PathBuf) -> Self {
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, responses) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let response = line
                    .map_err(|error| error.to_string())
                    .and_then(|line| serde_json::from_str(&line).map_err(|error| format!("{error}: {line}")));
                if sender.send(response).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            responses,
            reader: Some(reader),
            stderr,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
        let response = self.responses.recv_timeout(TIMEOUT).unwrap_or_else(|error| {
            let _ = self.child.kill();
            let _ = self.child.wait();
            panic!(
                "MCP reply failed: {error}; stderr={}",
                fs::read_to_string(&self.stderr).unwrap_or_default()
            );
        });
        let response = response.expect("stdout must contain only JSON-RPC objects, one per line");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        response
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name":name, "arguments":arguments}))
    }

    fn find(&mut self, arguments: Value) -> Value {
        successful(self.call("find_by_category", arguments))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if thread::panicking() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        } else {
            let status = wait_for_exit(&mut self.child);
            let errors = fs::read_to_string(&self.stderr).unwrap();
            assert!(status.success(), "MCP server failed: {errors}");
            assert!(errors.is_empty(), "{errors}");
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn successful(response: Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    let result = &response["result"];
    assert_eq!(result["isError"], false, "{response}");
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    assert_eq!(result["content"][0]["type"], "text");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    assert!(text["unreadable"].is_u64());
    assert!(text.get("cached_at").is_some());
    assert!(text["cached_at"].is_null() || text["cached_at"].is_u64());
    assert!(text["accounting"].is_object());
    assert_native_accounting(&text["accounting"]);
    if let Some(items) = text["items"].as_array() {
        let matched = text["matched_items"].as_u64().unwrap();
        assert!(matched >= items.len() as u64);
        assert_eq!(text["items_truncated"], matched > items.len() as u64);
    }
    text
}

fn invalid_arguments(response: Value) {
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert!(response.get("result").is_none(), "{response}");
}

fn execution_error(response: &Value) -> &Value {
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], true, "{response}");
    &response["result"]
}

fn stale_cursor(response: Value) {
    let result = execution_error(&response);
    assert_eq!(result["structuredContent"]["code"], "stale_cursor", "{response}");
    assert_eq!(result["structuredContent"]["restart_required"], true);
    assert!(
        result["structuredContent"]["message"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
}

fn start(root: &Path, category: &str, top: usize) -> Value {
    json!({"path":root, "category":category, "top":top, "paginate":true})
}

fn continuation(page: &Value, top: usize) -> Value {
    json!({
        "path":page["path"], "category":page["category"], "top":top,
        "cursor":page["pagination"]["next_cursor"].as_str().expect("page must have a cursor")
    })
}

fn assert_page(page: &Value, offset: u64, count: usize, total: u64, more: bool) {
    assert_eq!(page["items"].as_array().unwrap().len(), count);
    assert_eq!(page["matched_items"], total);
    assert_eq!(page["items_truncated"], total > count as u64);
    let pagination = &page["pagination"];
    assert!(pagination["snapshot_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(pagination["offset"].is_u64());
    assert_eq!(pagination["offset"], offset);
    assert_eq!(pagination["has_more"], more);
    assert!(pagination.get("next_cursor").is_some());
    if more {
        assert!(
            pagination["next_cursor"]
                .as_str()
                .is_some_and(|cursor| !cursor.is_empty())
        );
    } else {
        assert!(pagination["next_cursor"].is_null());
    }
}

fn inventory(server: &mut Server, first: Value, top: usize) -> (Vec<Value>, Vec<usize>) {
    let mut page = server.find(first);
    let total = page["matched_items"].as_u64().unwrap();
    let snapshot_id = page["pagination"]["snapshot_id"].clone();
    let accounting = page["accounting"].clone();
    let unreadable = page["unreadable"].clone();
    let mut items = Vec::new();
    let mut lengths = Vec::new();
    loop {
        let count = usize::try_from((total - items.len() as u64).min(top as u64)).unwrap();
        let more = items.len() as u64 + (count as u64) < total;
        assert_page(&page, items.len() as u64, count, total, more);
        assert_eq!(page["pagination"]["snapshot_id"], snapshot_id);
        assert_eq!(page["accounting"], accounting);
        assert_eq!(page["unreadable"], unreadable);
        items.extend_from_slice(page["items"].as_array().unwrap());
        lengths.push(count);
        if !more {
            break;
        }
        page = server.find(continuation(&page, top));
        assert!(page["cached_at"].is_u64());
    }
    (items, lengths)
}

#[test]
fn six_hundred_packages_match_full_cli_at_one_and_four_threads() {
    let fixture = Fixture::new();
    for i in 0..600 {
        let path = fixture.root.join(format!("p{i:03}/node_modules"));
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("payload.js"), b"x").unwrap();
    }
    let expected = fixture.cli(&fixture.root, Some("node_modules"), 600, &[], "1");
    assert_eq!(expected["matched_items"], 600);
    assert_eq!(expected["items_truncated"], false);
    assert_eq!(
        expected,
        fixture.cli(&fixture.root, Some("node_modules"), 600, &[], "4")
    );
    for threads in ["1", "4"] {
        let mut server = fixture.server(&["--cache"], threads);
        let legacy = server.find(json!({"path":fixture.root, "category":"node_modules"}));
        assert_eq!(legacy["items"].as_array().unwrap().len(), 50);
        assert_eq!(legacy["matched_items"], 600);
        assert_eq!(legacy["items_truncated"], true);
        assert!(legacy.get("pagination").is_none());
        let explicit_legacy = server.find(json!({
            "path":fixture.root, "category":"node_modules", "top":500, "paginate":false
        }));
        assert!(explicit_legacy.get("pagination").is_none());
        assert_eq!(
            explicit_legacy["items"],
            json!(expected["items"].as_array().unwrap()[..500])
        );
        for paginate in [false, true] {
            invalid_arguments(server.call(
                "find_by_category",
                json!({
                    "path":fixture.root, "category":"node_modules", "top":501, "paginate":paginate
                }),
            ));
        }
        for (top, lengths) in [(500, vec![500, 100]), (137, vec![137, 137, 137, 137, 52])] {
            let (items, actual_lengths) = inventory(&mut server, start(&fixture.root, "node_modules", top), top);
            assert_eq!(actual_lengths, lengths);
            assert_eq!(json!(items), expected["items"]);
        }
    }
    fixture.no_disk_cache();
}

#[test]
fn cursor_replay_page_size_changes_aliases_and_default_page_size() {
    let fixture = Fixture::new();
    fixture.pdfs(61);
    let expected = fixture.cli(&fixture.root, Some("PDF"), 100, &[], "2");
    let mut server = fixture.server(&["--cache"], "2");
    let defaults = server.find(json!({"path":"tree", "category":"pdf", "paginate":true}));
    assert_page(&defaults, 0, 50, 61, true);
    assert_eq!(
        defaults["path"],
        fs::canonicalize(&fixture.root).unwrap().display().to_string()
    );
    assert!(defaults["cached_at"].is_null());
    let mut default_continuation = continuation(&defaults, 1);
    default_continuation.as_object_mut().unwrap().remove("top");
    let default_last = server.find(default_continuation);
    assert_page(&default_last, 50, 11, 61, false);
    assert_eq!(
        default_last["items"],
        json!(expected["items"].as_array().unwrap()[50..])
    );
    let first = server.find(start(&fixture.root, "pDf", 2));
    let args = continuation(&first, 3);
    let second = server.find(args.clone());
    assert_eq!(second, server.find(args.clone()), "cursor replay must be idempotent");
    assert_page(&second, 2, 3, 61, true);
    assert_eq!(second["items"], json!(expected["items"].as_array().unwrap()[2..5]));
    let mut alias = args;
    alias["category"] = json!("pdf");
    alias["paginate"] = json!(true);
    alias["top"] = json!(4);
    let longer = server.find(alias);
    assert_page(&longer, 2, 4, 61, true);
    assert_eq!(longer["items"], json!(expected["items"].as_array().unwrap()[2..6]));
    let terminal = server.find(continuation(&longer, 100));
    assert_page(&terminal, 6, 55, 61, false);
    assert_eq!(terminal["items"], json!(expected["items"].as_array().unwrap()[6..]));
    assert_eq!(
        terminal["pagination"]["snapshot_id"],
        first["pagination"]["snapshot_id"]
    );
    fixture.no_disk_cache();
}

#[test]
fn empty_zero_byte_inherited_and_pruned_matches_keep_legacy_meanings() {
    let fixture = Fixture::new();
    for relative in ["node_modules/pkg/node_modules", "bower_components"] {
        fs::create_dir_all(fixture.root.join(relative)).unwrap();
        fs::write(fixture.root.join(relative).join("inherited.pdf"), b"").unwrap();
    }
    for name in ["a.pdf", "z.pdf"] {
        fs::write(fixture.root.join(name), b"").unwrap();
    }
    let mut server = fixture.server(&["--cache"], "2");
    let (directories, lengths) = inventory(&mut server, start(&fixture.root, "node_modules", 1), 1);
    assert_eq!(lengths, [1, 1]);
    assert_eq!(directories.len(), 2, "nested matching directories must be pruned");
    assert!(
        directories
            .iter()
            .all(|item| !item["path"].as_str().unwrap().contains("pkg"))
    );
    let (pdfs, _) = inventory(&mut server, start(&fixture.root, "PDF", 1), 1);
    assert_eq!(pdfs.len(), 2, "files inside special directories inherit their category");
    assert!(pdfs.iter().all(|item| item["size_bytes"] == 0));
    assert!(pdfs[0]["path"].as_str().unwrap().ends_with("z.pdf"));
    assert!(pdfs[1]["path"].as_str().unwrap().ends_with("a.pdf"));
    let empty = server.find(start(&fixture.root, "Video", 1));
    assert_page(&empty, 0, 0, 0, false);
    let root_match = server.find(start(&fixture.root.join("bower_components"), "node_modules", 1));
    assert_page(&root_match, 0, 0, 0, false);
    fixture.no_disk_cache();
}

#[test]
fn schema_and_argument_errors_are_limited_to_category_pagination() {
    let fixture = Fixture::new();
    fixture.pdfs(3);
    let mut server = fixture.server(&["--cache"], "2");
    let schema = server.request("tools/list", json!({}));
    let tools = schema["result"]["tools"].as_array().unwrap();
    let find = tools.iter().find(|tool| tool["name"] == "find_by_category").unwrap();
    let usage = tools.iter().find(|tool| tool["name"] == "disk_usage").unwrap();
    assert_eq!(find["inputSchema"]["properties"]["paginate"]["type"], "boolean");
    assert_eq!(find["inputSchema"]["properties"]["cursor"]["type"], "string");
    for key in ["paginate", "cursor"] {
        assert!(usage["inputSchema"]["properties"].get(key).is_none());
    }
    // A nonexistent root ensures these errors precede path resolution/scanning.
    for (key, value) in [
        ("paginate", json!(null)),
        ("paginate", json!(1)),
        ("paginate", json!("true")),
        ("cursor", json!(null)),
        ("cursor", json!(false)),
        ("cursor", json!([])),
        ("cursor", json!("")),
        ("cursor", json!("not-a-cursor")),
        ("cursor", json!("{\"version\":999}")),
    ] {
        let mut args = start(&fixture.root.join("missing"), "PDF", 1);
        args[key] = value;
        invalid_arguments(server.call("find_by_category", args));
    }
    for (key, value) in [
        ("paginate", json!(true)),
        ("paginate", json!(false)),
        ("cursor", json!("bad")),
    ] {
        let mut args = json!({"path":fixture.root.join("missing")});
        args[key] = value;
        invalid_arguments(server.call("disk_usage", args));
    }
    let first = server.find(start(&fixture.root, "PDF", 1));
    for (key, value) in [("refresh", json!(true)), ("paginate", json!(false))] {
        let mut args = continuation(&first, 1);
        args[key] = value;
        invalid_arguments(server.call("find_by_category", args));
    }
    let next = server.find(continuation(&first, 1));
    assert_page(&next, 1, 1, 3, true);
    fixture.no_disk_cache();
}

#[test]
fn cursor_binds_canonical_root_category_and_effective_query_options() {
    let fixture = Fixture::new();
    fixture.pdfs(4);
    let mut server = fixture.server(&["--cache"], "2");
    let first = server.find(start(&fixture.root, "PDF", 1));
    for (key, value) in [
        ("path", json!(fixture.root.join("missing"))),
        ("path", json!("tree")),
        ("path", json!(format!("{}/.", first["path"].as_str().unwrap()))),
        ("category", json!("Video")),
        ("size_mode", json!("dedup")),
        ("clones", json!(true)),
        ("private", json!(true)),
        ("cross_fs", json!(true)),
    ] {
        let mut args = continuation(&first, 1);
        args[key] = value;
        invalid_arguments(server.call("find_by_category", args));
    }
    assert_page(&server.find(continuation(&first, 1)), 1, 1, 4, true);
    let mut nondefault = start(&fixture.root, "PDF", 1);
    nondefault["size_mode"] = json!("dedup");
    nondefault["private"] = json!(true);
    nondefault["cross_fs"] = json!(true);
    let advanced = server.find(nondefault);
    let mut args = continuation(&advanced, 2);
    invalid_arguments(server.call("find_by_category", args.clone()));
    args["size_mode"] = json!("dedup");
    args["private"] = json!(true);
    args["cross_fs"] = json!(true);
    // Explicit false still has effective clones=true because dedup/private imply it.
    args["clones"] = json!(false);
    let next = server.find(args.clone());
    assert_page(&next, 1, 2, 4, true);
    args["clones"] = json!(true);
    assert_eq!(next, server.find(args));
    fixture.no_disk_cache();
}

#[test]
fn pagination_requires_cache_without_server_wide_refresh() {
    let fixture = Fixture::new();
    fixture.pdfs(2);
    let first = {
        let mut server = fixture.server(&["--cache"], "2");
        server.find(start(&fixture.root, "PDF", 1))
    };
    for options in [&[][..], &["--refresh"][..], &["--cache", "--refresh"][..]] {
        let mut server = fixture.server(options, "2");
        for args in [start(&fixture.root, "PDF", 1), continuation(&first, 1)] {
            let response = server.call("find_by_category", args);
            let result = execution_error(&response);
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("--cache"), "{response}");
            assert!(text.contains("--refresh"), "{response}");
        }
        let legacy = server.find(json!({"path":fixture.root, "category":"PDF"}));
        assert!(legacy.get("pagination").is_none());
    }
    fixture.no_disk_cache();
}

#[test]
fn mutations_root_removal_and_failed_refresh_preserve_the_retained_generation() {
    let fixture = Fixture::new();
    fixture.pdfs(5);
    let expected = fixture.cli(&fixture.root, Some("PDF"), 10, &[], "2");
    let mut server = fixture.server(&["--cache"], "2");
    let first = server.find(start(&fixture.root, "PDF", 2));
    let args = continuation(&first, 10);
    fs::remove_file(fixture.root.join("0000.pdf")).unwrap();
    fs::write(fixture.root.join("new.pdf"), vec![0; 16_384]).unwrap();
    let second = server.find(args.clone());
    assert_page(&second, 2, 3, 5, false);
    assert_eq!(second["items"], json!(expected["items"].as_array().unwrap()[2..]));
    assert_eq!(second["accounting"], first["accounting"]);
    fs::rename(&fixture.root, fixture.base.join("renamed-tree")).unwrap();
    assert_eq!(
        second,
        server.find(args.clone()),
        "continuations must not canonicalize a removed root"
    );
    for tool in ["disk_usage", "find_by_category"] {
        let response = server.call(
            tool,
            json!({
                "path":first["path"], "category":"PDF", "refresh":true
            }),
        );
        execution_error(&response);
        assert_eq!(
            second,
            server.find(args.clone()),
            "failed refresh must not evict the old tree"
        );
    }
    fixture.no_disk_cache();
}

#[test]
fn either_tool_refresh_and_capability_upgrades_invalidate_old_cursors() {
    let fixture = Fixture::new();
    fixture.pdfs(4);
    let mut server = fixture.server(&["--cache"], "2");
    let mut previous = server.find(start(&fixture.root, "PDF", 1));
    for tool in ["disk_usage", "find_by_category"] {
        successful(server.call(tool, json!({"path":fixture.root, "category":"PDF", "refresh":true})));
        stale_cursor(server.call("find_by_category", continuation(&previous, 1)));
        let next = server.find(start(&fixture.root, "PDF", 1));
        assert_ne!(next["pagination"]["snapshot_id"], previous["pagination"]["snapshot_id"]);
        previous = next;
    }
    // Compatible reads retain the generation; stronger capability scans replace it.
    successful(server.call("disk_usage", json!({"path":fixture.root})));
    assert_page(&server.find(continuation(&previous, 1)), 1, 1, 4, true);
    successful(server.call("disk_usage", json!({"path":fixture.root, "clones":true})));
    stale_cursor(server.call("find_by_category", continuation(&previous, 1)));
    let mut args = start(&fixture.root, "PDF", 1);
    args["clones"] = json!(true);
    let with_clones = server.find(args);
    successful(server.call("disk_usage", json!({"path":fixture.root, "private":true})));
    let mut old_cursor = continuation(&with_clones, 1);
    old_cursor["clones"] = json!(true);
    stale_cursor(server.call("find_by_category", old_cursor));
    fixture.no_disk_cache();
}

#[test]
fn independent_roots_and_filesystem_policies_do_not_invalidate_each_other() {
    let fixture = Fixture::new();
    fixture.pdfs(3);
    let other = fixture.base.join("other");
    fs::create_dir(&other).unwrap();
    for name in ["a.pdf", "b.pdf"] {
        fs::write(other.join(name), b"").unwrap();
    }
    let mut server = fixture.server(&["--cache"], "2");
    let first = server.find(start(&fixture.root, "PDF", 1));
    let other_first = server.find(start(&other, "PDF", 1));
    assert_ne!(
        first["pagination"]["snapshot_id"],
        other_first["pagination"]["snapshot_id"]
    );
    successful(server.call("disk_usage", json!({"path":other, "refresh":true})));
    stale_cursor(server.call("find_by_category", continuation(&other_first, 1)));
    assert_page(&server.find(continuation(&first, 1)), 1, 1, 3, true);
    let mut cross_fs = start(&fixture.root, "PDF", 1);
    cross_fs["cross_fs"] = json!(true);
    let cross_first = server.find(cross_fs);
    successful(server.call(
        "disk_usage",
        json!({"path":fixture.root, "cross_fs":true, "refresh":true}),
    ));
    let mut cross_cursor = continuation(&cross_first, 1);
    cross_cursor["cross_fs"] = json!(true);
    stale_cursor(server.call("find_by_category", cross_cursor));
    assert_page(&server.find(continuation(&first, 1)), 1, 1, 3, true);
    fixture.no_disk_cache();
}

fn cache_files(fixture: &Fixture) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<_> = fs::read_dir(fixture.base.join("cache"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn disk_cache_rewrites_do_not_change_live_cursors_and_restart_always_invalidates_them() {
    let fixture = Fixture::new();
    fixture.pdfs(4);
    let initial = fixture.cli(&fixture.root, Some("PDF"), 100, &["--cache"], "2");
    let original_disk = cache_files(&fixture);
    assert_eq!(original_disk.len(), 1);
    let mut server = fixture.server(&["--cache"], "2");
    let first = server.find(start(&fixture.root, "PDF", 1));
    assert!(
        first["cached_at"].is_u64(),
        "first request should load the CLI snapshot"
    );
    // Reloading the identical disk snapshot still creates a new process generation.
    {
        let mut restarted = fixture.server(&["--cache"], "2");
        stale_cursor(restarted.call("find_by_category", continuation(&first, 1)));
        let same_disk = restarted.find(start(&fixture.root, "PDF", 1));
        assert_ne!(
            same_disk["pagination"]["snapshot_id"],
            first["pagination"]["snapshot_id"]
        );
        assert_eq!(same_disk["items"], first["items"]);
    }
    fs::write(fixture.root.join("new.pdf"), b"").unwrap();
    let rewritten = fixture.cli(&fixture.root, Some("PDF"), 100, &["--refresh"], "2");
    assert_eq!(rewritten["matched_items"], 5);
    let rewritten_disk = cache_files(&fixture);
    assert_ne!(rewritten_disk, original_disk);
    let still_old = server.find(continuation(&first, 100));
    assert_page(&still_old, 1, 3, 4, false);
    assert_eq!(still_old["items"], json!(initial["items"].as_array().unwrap()[1..]));
    fs::write(fixture.root.join("newer.pdf"), b"").unwrap();
    let mut refresh = start(&fixture.root, "PDF", 1);
    refresh["refresh"] = json!(true);
    let fresh = server.find(refresh);
    assert_page(&fresh, 0, 1, 6, true);
    assert_ne!(fresh["pagination"]["snapshot_id"], first["pagination"]["snapshot_id"]);
    stale_cursor(server.call("find_by_category", continuation(&first, 1)));
    assert_eq!(
        cache_files(&fixture),
        rewritten_disk,
        "MCP refresh must not overwrite CLI cache"
    );
    drop(server);
    let mut restarted = fixture.server(&["--cache"], "2");
    stale_cursor(restarted.call("find_by_category", continuation(&fresh, 1)));
    let latest_disk = restarted.find(start(&fixture.root, "PDF", 1));
    assert_page(&latest_disk, 0, 1, 5, true);
    assert_ne!(
        latest_disk["pagination"]["snapshot_id"],
        fresh["pagination"]["snapshot_id"]
    );
    assert_eq!(cache_files(&fixture), rewritten_disk);
}

// Some Unix filesystems (notably APFS) reject invalid UTF-8 names. Apply
// this capability guard only to the first invalid-name fixture operation,
// never to subsequent filesystem operations or the behavior under test.
#[cfg(unix)]
fn invalid_name_fixture_supported(result: std::io::Result<()>) -> std::io::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!("skipping invalid-name fixture: this filesystem rejects non-UTF-8 names (EILSEQ)");
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
#[test]
fn invalid_name_fixture_guard_skips_only_eilseq() {
    assert!(invalid_name_fixture_supported(Ok(())).unwrap());
    assert!(!invalid_name_fixture_supported(Err(std::io::Error::from_raw_os_error(libc::EILSEQ))).unwrap());
    for code in [libc::EACCES, libc::ENOENT, libc::ENOSPC, libc::ENAMETOOLONG] {
        let error = invalid_name_fixture_supported(Err(std::io::Error::from_raw_os_error(code))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(code));
    }
    let error = invalid_name_fixture_supported(Err(std::io::Error::other("fixture failure"))).unwrap_err();
    assert_eq!(error.to_string(), "fixture failure");
}

#[cfg(unix)]
#[test]
fn continuation_uses_emitted_path_for_non_utf8_canonical_roots_without_filesystem_lookup() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.pdfs(3);
    let raw_root = fixture.base.join(OsStr::from_bytes(b"raw\xff-root"));
    if !invalid_name_fixture_supported(fs::rename(&fixture.root, &raw_root)).unwrap() {
        return;
    }
    let entrypoint = fixture.base.join("readable-link");
    symlink(&raw_root, &entrypoint).unwrap();
    let canonical = fs::canonicalize(&entrypoint).unwrap();
    assert!(canonical.to_str().is_none());
    let expected = fixture.cli(&entrypoint, Some("PDF"), 10, &[], "2");
    let mut server = fixture.server(&["--cache"], "2");
    let first = server.find(start(&entrypoint, "PDF", 1));
    assert_page(&first, 0, 1, 3, true);
    assert_eq!(first["path"], canonical.display().to_string());
    let args = continuation(&first, 10);
    fs::remove_file(&entrypoint).unwrap();
    fs::rename(&raw_root, fixture.base.join("moved-root")).unwrap();
    let rest = server.find(args.clone());
    assert_page(&rest, 1, 2, 3, false);
    assert_eq!(rest["path"], first["path"]);
    assert_eq!(rest["pagination"]["snapshot_id"], first["pagination"]["snapshot_id"]);
    assert_eq!(rest["items"], json!(expected["items"].as_array().unwrap()[1..]));
    assert_eq!(rest, server.find(args));
    fixture.no_disk_cache();
}

#[cfg(unix)]
#[test]
fn equal_displayed_paths_have_distinct_cursor_boundaries() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new();
    if !invalid_name_fixture_supported(fs::write(fixture.root.join(OsStr::from_bytes(b"bad\xff.pdf")), b"")).unwrap() {
        return;
    }
    fs::write(fixture.root.join("bad\\xff.pdf"), b"").unwrap();
    let expected = fixture.cli(&fixture.root, Some("PDF"), 10, &[], "2");
    assert_eq!(expected["matched_items"], 2);
    assert_eq!(expected["items"][0], expected["items"][1]);
    let mut server = fixture.server(&["--cache"], "2");
    let first = server.find(start(&fixture.root, "PDF", 1));
    assert_page(&first, 0, 1, 2, true);
    let args = continuation(&first, 1);
    let second = server.find(args.clone());
    assert_page(&second, 1, 1, 2, false);
    assert_eq!(
        first["items"], second["items"],
        "both snapshot nodes must be returned despite identical rendering"
    );
    assert_eq!(second, server.find(args));
    fixture.no_disk_cache();
}

#[cfg(unix)]
#[test]
fn whole_root_hardlink_attribution_is_fixed_across_pages_and_mutation() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    for i in 0..3 {
        fs::create_dir_all(fixture.root.join(format!("p{i}/node_modules"))).unwrap();
    }
    let champion = fixture.root.join("champion.pdf");
    let owner = fixture.root.join("p1/node_modules/owner.bin");
    fs::write(&champion, vec![0; 8192]).unwrap();
    fs::write(&owner, vec![1; 8192]).unwrap();
    fs::hard_link(&champion, fixture.root.join("p0/node_modules/alias.pdf")).unwrap();
    fs::hard_link(&owner, fixture.root.join("p2/node_modules/alias.bin")).unwrap();
    let expected = fixture.cli(&fixture.root, Some("node_modules"), 10, &[], "1");
    assert_eq!(expected, fixture.cli(&fixture.root, Some("node_modules"), 10, &[], "4"));
    let whole = expected["items"].as_array().unwrap();
    for (project, file_bytes) in [(0, 0), (1, fs::metadata(&owner).unwrap().blocks() * 512), (2, 0)] {
        let directory = fs::canonicalize(fixture.root.join(format!("p{project}/node_modules"))).unwrap();
        let item = whole
            .iter()
            .find(|item| item["path"] == directory.display().to_string())
            .unwrap();
        assert_eq!(
            item["size_bytes"],
            fs::metadata(&directory).unwrap().blocks() * 512 + file_bytes
        );
    }
    // Separately scanning sibling roots credits each inode independently; pagination
    // must instead keep the original combined-root attribution throughout.
    for project in [1, 2] {
        let directory = fixture.root.join(format!("p{project}/node_modules"));
        let separate = fixture.cli(&directory, None, 10, &[], "2");
        assert_eq!(
            separate["size_bytes"],
            fs::metadata(&directory).unwrap().blocks() * 512 + fs::metadata(&owner).unwrap().blocks() * 512
        );
    }
    let mut server = fixture.server(&["--cache"], "4");
    let first = server.find(start(&fixture.root, "node_modules", 1));
    fs::remove_file(champion).unwrap();
    let rest = server.find(continuation(&first, 10));
    assert_page(&rest, 1, 2, 3, false);
    let mut items = first["items"].as_array().unwrap().clone();
    items.extend_from_slice(rest["items"].as_array().unwrap());
    assert_eq!(json!(items), expected["items"]);
    assert_eq!(rest["accounting"], first["accounting"]);
    fixture.no_disk_cache();
}

#[cfg(unix)]
#[test]
fn scan_wide_unreadable_warning_survives_each_page_and_permission_recovery() {
    use std::os::unix::fs::PermissionsExt;

    struct RestorePermissions(PathBuf, fs::Permissions);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, self.1.clone());
        }
    }

    let fixture = Fixture::new();
    fixture.pdfs(3);
    let denied = fixture.root.join("denied");
    fs::create_dir(&denied).unwrap();
    fs::write(denied.join("hidden.pdf"), b"").unwrap();
    let permissions = RestorePermissions(denied.clone(), fs::metadata(&denied).unwrap().permissions());
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
    match fs::read_dir(&denied) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
        Ok(_) => {
            eprintln!("SKIP unreadable pagination assertions: this process can read chmod(000) directories");
            return;
        }
        Err(error) => panic!("expected PermissionDenied, got {error}"),
    }
    let mut server = fixture.server(&["--cache"], "2");
    let first = server.find(start(&fixture.root, "PDF", 1));
    assert_page(&first, 0, 1, 3, true);
    assert_eq!(first["unreadable"], 1);
    drop(permissions);
    let second = server.find(continuation(&first, 1));
    assert_page(&second, 1, 1, 3, true);
    assert_eq!(second["unreadable"], 1);
    let last = server.find(continuation(&second, 1));
    assert_page(&last, 2, 1, 3, false);
    assert_eq!(last["unreadable"], 1);
    let mut args = start(&fixture.root, "PDF", 1);
    args["refresh"] = json!(true);
    let refreshed = server.find(args);
    assert_page(&refreshed, 0, 1, 4, true);
    assert_eq!(refreshed["unreadable"], 0);
    stale_cursor(server.call("find_by_category", continuation(&first, 1)));
    fixture.no_disk_cache();
}

#[test]
fn mixed_inventory_pages_partition_606_matches_and_bind_include_files() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.config,
        r#"
        [[category]]
        name = "Unity"
        dirs = ["Library", "Temp"]
        markers = ["ProjectSettings"]
        extensions = ["unitypackage"]
    "#,
    )
    .unwrap();
    for i in 0..303 {
        let directory = fixture.root.join(format!("p{i:03}/Library/Temp"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("pruned.unitypackage"), b"").unwrap();
        // Ties, zero-byte matches, and pages that interleave directory/file ranks.
        fs::write(
            fixture.root.join(format!("{i:03}.unitypackage")),
            vec![b'x'; if i % 2 == 0 { 16384 } else { 0 }],
        )
        .unwrap();
    }
    for mode in ["du", "dedup"] {
        let expected = fixture.cli(
            &fixture.root,
            Some("Unity"),
            1000,
            &["--include-files", "--size-mode", mode],
            "1",
        );
        assert_eq!(expected["matched_items"], 606);
        assert_eq!(expected["items_truncated"], false);
        assert_eq!(
            expected,
            fixture.cli(
                &fixture.root,
                Some("Unity"),
                1000,
                &["--include-files", "--size-mode", mode],
                "4"
            )
        );
        for threads in ["1", "4"] {
            let mut server = fixture.server(&["--cache"], threads);
            let base = json!({"path":fixture.root, "category":"Unity", "size_mode":mode});
            // Prime the retained snapshot before exact default/explicit-false comparison.
            server.find(base.clone());
            let legacy = server.find(base.clone());
            let mut explicit_false = base.clone();
            explicit_false["include_files"] = json!(false);
            assert_eq!(legacy, server.find(explicit_false));
            assert_eq!(legacy["matched_items"], 303);
            assert!(legacy.get("include_files").is_none());
            for top in [500, 137] {
                let mut first = base.clone();
                first["top"] = json!(top);
                first["paginate"] = json!(true);
                first["include_files"] = json!(true);
                let mut page = server.find(first);
                let snapshot_id = page["pagination"]["snapshot_id"].clone();
                let mut all = Vec::new();
                loop {
                    let count = (606 - all.len()).min(top);
                    let more = all.len() + count < 606;
                    assert_page(&page, all.len() as u64, count, 606, more);
                    assert_eq!(page["include_files"], true);
                    assert_eq!(page["accounting"], expected["accounting"]);
                    assert_eq!(page["pagination"]["snapshot_id"], snapshot_id);
                    all.extend_from_slice(page["items"].as_array().unwrap());
                    if !more {
                        break;
                    }
                    let mut next = continuation(&page, top);
                    next["size_mode"] = json!(mode);
                    invalid_arguments(server.call("find_by_category", next.clone()));
                    next["include_files"] = json!(false);
                    invalid_arguments(server.call("find_by_category", next.clone()));
                    next["include_files"] = json!(true);
                    let following = server.find(next.clone());
                    assert_eq!(following, server.find(next));
                    page = following;
                }
                assert_eq!(json!(all), expected["items"]);
            }
        }
    }
    fixture.no_disk_cache();
}

#[test]
fn include_files_schema_type_validation_and_pure_category_cursor_binding() {
    let fixture = Fixture::new();
    fixture.pdfs(3);
    let mut server = fixture.server(&["--cache"], "1");
    let schema = server.request("tools/list", json!({}));
    let tools = schema["result"]["tools"].as_array().unwrap();
    for tool in tools {
        let property = &tool["inputSchema"]["properties"]["include_files"];
        if tool["name"] == "find_by_category" {
            assert_eq!(property["type"], "boolean");
            assert_eq!(property["default"], false);
        } else {
            assert!(property.is_null());
        }
    }
    for invalid in [json!(null), json!(1), json!("true"), json!([]), json!({})] {
        invalid_arguments(server.call(
            "find_by_category",
            json!({"path":fixture.root, "category":"PDF", "include_files":invalid}),
        ));
    }
    for value in [true, false] {
        invalid_arguments(server.call("disk_usage", json!({"path":fixture.root, "include_files":value})));
    }
    let first = server.find(start(&fixture.root, "PDF", 1));
    let mut changed = continuation(&first, 1);
    changed["include_files"] = json!(true);
    invalid_arguments(server.call("find_by_category", changed));
    let mut opted = start(&fixture.root, "PDF", 1);
    opted["include_files"] = json!(true);
    let opted = server.find(opted);
    assert_eq!(first["items"], opted["items"]);
    assert_eq!(first["matched_items"], opted["matched_items"]);
    invalid_arguments(server.call("find_by_category", continuation(&opted, 1)));
    let mut next = continuation(&opted, 1);
    next["include_files"] = json!(true);
    assert_page(&server.find(next), 1, 1, 3, true);
}

// The fields describe native scan policy even when no duplicates exist. Cache
// parity here deliberately uses a snapshot produced on the same platform.
#[test]
fn native_accounting_matches_cli_cache_mcp_and_pages() {
    let fixture = Fixture::new();
    let owner = fixture.root.join("a.pdf");
    fs::write(&owner, b"1234567").unwrap();
    fs::hard_link(&owner, fixture.root.join("z.pdf")).unwrap();
    for options in [vec![], vec!["--clones"], vec!["--private"]] {
        let full = fixture.cli(&fixture.root, None, 10, &options, "1");
        let category = fixture.cli(&fixture.root, Some("PDF"), 10, &options, "4");
        let accounting = &full["accounting"];
        assert_native_accounting(accounting);
        assert_eq!(&category["accounting"], accounting);
        let items = category["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(category["items"], full["largest_files"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let allocated = fs::metadata(&owner).unwrap().blocks() * 512;
            for item in items {
                let name = Path::new(item["path"].as_str().unwrap()).file_name().unwrap();
                let expected = if name == "a.pdf" { allocated } else { 0 };
                assert_eq!(item["size_bytes"], expected);
            }
            assert_eq!(
                full["size_bytes"],
                allocated + fs::metadata(&fixture.root).unwrap().blocks() * 512
            );
        }
        #[cfg(not(unix))]
        {
            assert_eq!(accounting["private"], false);
            assert_eq!(full["size_bytes"], 14);
            assert_eq!(full["dedup_bytes"], 14);
            assert!(full["freeable_min_bytes"].is_null());
            for item in items.iter().chain(full["children"].as_array().unwrap()) {
                assert_eq!(item["size_bytes"], 7);
                assert_eq!(item["dedup_bytes"], 7);
                assert!(item["freeable_min_bytes"].is_null());
                assert!(item.get("private_bytes").is_none());
            }
        }
        // Replace the cache for each option set, then verify actual cache hits.
        let mut refresh = options.clone();
        refresh.extend(["--cache", "--refresh"]);
        let fresh = fixture.cli(&fixture.root, None, 10, &refresh, "2");
        assert!(fresh["cached_at"].is_null());
        assert_eq!(&fresh["accounting"], accounting);
        let mut cached_options = options.clone();
        cached_options.push("--cache");
        for cat in [None, Some("PDF")] {
            let cached = fixture.cli(&fixture.root, cat, 10, &cached_options, "2");
            assert!(cached["cached_at"].is_u64());
            assert_eq!(&cached["accounting"], accounting);
        }
        let mut server = fixture.server(&cached_options, "4");
        let clones = options.contains(&"--clones");
        let private = options.contains(&"--private");
        let args = json!({"path": fixture.root, "refresh": true, "clones": clones, "private": private});
        let mcp_full = successful(server.call("disk_usage", args));
        assert_eq!(&mcp_full["accounting"], accounting);
        let mcp_category =
            server.find(json!({"path": fixture.root, "category": "PDF", "clones": clones, "private": private}));
        assert_eq!(&mcp_category["accounting"], accounting);
        assert_eq!(mcp_category["items"], category["items"]);
        let mut args = start(&fixture.root, "PDF", 1);
        args["clones"] = clones.into();
        args["private"] = private.into();
        let first = server.find(args);
        let mut args = continuation(&first, 1);
        args["clones"] = clones.into();
        args["private"] = private.into();
        let second = server.find(args);
        assert_page(&first, 0, 1, 2, true);
        assert_page(&second, 1, 1, 2, false);
        for page in [&first, &second] {
            assert_eq!(&page["accounting"], accounting);
        }
        assert_eq!(json!([first["items"][0], second["items"][0]]), category["items"]);
    }
}

fn assert_native_accounting(accounting: &Value) {
    assert_eq!(accounting["size"], "du-allocated");
    #[cfg(unix)]
    let expected = ("allocated", "count-once");
    #[cfg(not(unix))]
    let expected = ("logical", "unsupported");
    assert_eq!(accounting["measurement_basis"], expected.0);
    assert_eq!(accounting["hardlink_deduplication"], expected.1);
}
