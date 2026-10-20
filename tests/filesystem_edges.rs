//! Black-box filesystem regressions. Every scan runs in a subprocess with a
//! deadline, so a traversal loop cannot hang the test runner.

use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCAN_TIMEOUT: Duration = Duration::from_secs(20);
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!(
            "dutype-edge-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("tree");
        fs::create_dir_all(&root).unwrap();
        let config = base.join("config.toml");
        // Keep the invoking user's category configuration out of the test.
        fs::write(&config, "").unwrap();
        Self { base, root, config }
    }

    fn command(&self) -> Command {
        self.command_for("--json")
    }

    fn command_for(&self, format: &str) -> Command {
        self.command_for_top(format, 100)
    }

    fn command_for_top(&self, format: &str, top: usize) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dutype"));
        command
            .arg(format)
            .arg("--config")
            .arg(&self.config)
            .arg("--top")
            .arg(top.to_string())
            .arg(&self.root)
            .env("DUTYPE_CACHE_DIR", self.base.join("unused-cache"))
            .env("RAYON_NUM_THREADS", "4")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn output(&self, format: &str, args: &[&str]) -> String {
        self.output_with_top(format, 100, args)
    }

    fn output_with_top(&self, format: &str, top: usize, args: &[&str]) -> String {
        let output = bounded_output(self.command_for_top(format, top).args(args).spawn().unwrap());
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "report failed: status={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn category_report(&self, args: &[&str]) -> Value {
        self.category_report_with_top("PDF", 100, args)
    }

    fn category_report_with_top(&self, category: &str, top: usize, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.extend(["--category", category]);
        let report: Value = serde_json::from_str(&self.output_with_top("--json", top, &args)).unwrap();
        assert_absolute_path(&report);
        assert_eq!(report["category"], category);
        assert_u64(&report, "unreadable");
        assert_optional_u64(&report, "cached_at");
        assert!(report["accounting"].is_object());
        let shown = report["items"].as_array().unwrap().len() as u64;
        let matched = assert_u64(&report, "matched_items");
        assert!(matched >= shown);
        assert_eq!(report["items_truncated"], matched > shown);
        report
    }

    fn report(&self) -> Value {
        self.report_with_threads("4")
    }

    fn report_with_threads(&self, threads: &str) -> Value {
        let output = bounded_output(self.command().env("RAYON_NUM_THREADS", threads).spawn().unwrap());
        assert!(
            output.status.success(),
            "scan failed: status={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid JSON: {error}; stdout={}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        assert_report_schema(&report, &self.root);
        report
    }

    fn assert_stable(&self) -> Value {
        let mut expected = self.report_with_threads("1");
        expected.as_object_mut().unwrap().remove("scan_seconds");
        for threads in ["4", "2", "4"] {
            let mut actual = self.report_with_threads(threads);
            actual.as_object_mut().unwrap().remove("scan_seconds");
            assert_eq!(actual, expected, "JSON changed with {threads} scan threads");
        }
        expected
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn bounded_output(mut child: Child) -> Output {
    // Drain both pipes while waiting, rather than deadlocking if a report
    // exceeds the operating system's pipe capacity.
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() >= SCAN_TIMEOUT {
            timed_out = true;
            let _ = child.kill();
            break child.wait().unwrap();
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout_reader.join().unwrap();
    let stderr = stderr_reader.join().unwrap();
    assert!(
        !timed_out,
        "scan exceeded {SCAN_TIMEOUT:?}; stderr={}",
        String::from_utf8_lossy(&stderr)
    );
    Output { status, stdout, stderr }
}

fn assert_u64(value: &Value, key: &str) -> u64 {
    value[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} is not an unsigned integer: {value}"))
}

fn assert_optional_u64(value: &Value, key: &str) {
    assert!(value.get(key).is_some(), "missing public field {key}");
    assert!(
        value[key].is_null() || value[key].as_u64().is_some(),
        "invalid {key}: {value}"
    );
}

fn assert_absolute_path(value: &Value) {
    let path = value["path"].as_str().expect("missing string path");
    assert!(Path::new(path).is_absolute(), "non-absolute public path: {path}");
}

fn assert_category_schema(category: &Value) {
    assert!(category["name"].is_string());
    assert!(category["size"].is_string());
    assert_u64(category, "size_bytes");
    let percent = category["percent"].as_f64().expect("missing finite percent");
    assert!(percent.is_finite() && (0.0..=100.0).contains(&percent));
}

fn assert_report_schema(report: &Value, root: &Path) {
    assert_absolute_path(report);
    for key in ["matched_items", "items_truncated"] {
        assert!(
            report.get(key).is_none(),
            "category-only field {key} leaked into full report"
        );
    }
    assert_eq!(
        report["path"].as_str().unwrap(),
        fs::canonicalize(root).unwrap().to_str().unwrap()
    );
    let size = assert_u64(report, "size_bytes");
    assert!(report["size"].is_string());
    assert_eq!(assert_u64(report, "dedup_bytes"), size);
    assert_eq!(assert_u64(report, "clone_shared_bytes"), 0);
    assert_optional_u64(report, "freeable_min_bytes");
    assert_u64(report, "files");
    assert_u64(report, "unreadable");
    let seconds = report["scan_seconds"].as_f64().expect("missing scan_seconds");
    assert!(seconds.is_finite() && seconds >= 0.0);
    assert_optional_u64(report, "cached_at");
    assert!(report["cached_at"].is_null(), "fixture must not use a cache");
    let accounting = &report["accounting"];
    assert_eq!(accounting["size"], "du-allocated");
    assert_eq!(accounting["size_mode"], "du");
    assert_eq!(accounting["clone_detection"], "off");
    assert_eq!(accounting["private"], false);
    assert!(accounting["caveats"].as_array().unwrap().iter().all(Value::is_string));

    let categories = report["categories"].as_array().expect("missing categories");
    for category in categories {
        assert_category_schema(category);
    }
    assert_eq!(
        categories
            .iter()
            .map(|c| c["size_bytes"].as_u64().unwrap())
            .sum::<u64>(),
        size,
        "category bytes must add up, even for a partial scan"
    );
    for child in report["children"].as_array().expect("missing children") {
        assert_absolute_path(child);
        assert!(child["name"].is_string());
        assert!(matches!(child["type"].as_str(), Some("file" | "dir")));
        assert!(child["size"].is_string());
        assert_u64(child, "size_bytes");
        assert_u64(child, "dedup_bytes");
        assert_u64(child, "files");
        assert_optional_u64(child, "freeable_min_bytes");
        assert!(child["clone"].is_boolean());
        assert!(child.get("special").is_some());
        assert!(child["special"].is_null() || child["special"].is_string());
        let percent = child["percent"].as_f64().expect("missing child percent");
        assert!(percent.is_finite() && (0.0..=100.0).contains(&percent));
        for category in child["top_categories"].as_array().expect("missing top_categories") {
            assert_category_schema(category);
        }
    }
    for key in ["special_dirs", "largest_files"] {
        for entry in report[key].as_array().unwrap_or_else(|| panic!("missing {key}")) {
            assert_absolute_path(entry);
            assert!(entry["category"].is_string());
            assert!(entry["size"].is_string());
            assert_u64(entry, "size_bytes");
            assert_u64(entry, "dedup_bytes");
            assert_optional_u64(entry, "freeable_min_bytes");
            assert!(entry["clone"].is_boolean());
        }
    }
}

#[test]
fn empty_directory_has_complete_stable_json() {
    let fixture = Fixture::new("empty");
    let report = fixture.assert_stable();
    assert_eq!(report["files"], 0);
    assert_eq!(report["unreadable"], 0);
    for key in ["children", "special_dirs", "largest_files"] {
        assert!(report[key].as_array().unwrap().is_empty());
    }
    // An empty directory can still have allocated blocks. Do not incorrectly
    // require size_bytes or the categories array to be zero/empty on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            report["size_bytes"],
            fs::metadata(&fixture.root).unwrap().blocks() * 512
        );
    }
}

#[test]
fn missing_category_keeps_zero_errors_explicit_in_fresh_and_cached_reports() {
    let fixture = Fixture::new("missing-category");
    fs::write(fixture.root.join("visible.txt"), b"not a PDF").unwrap();
    let full = fixture.report();
    let fresh = fixture.category_report(&[]);
    assert_eq!(fresh["unreadable"], full["unreadable"]);
    assert_eq!(fresh["unreadable"], 0);
    assert!(fresh["cached_at"].is_null());
    assert!(fresh["items"].as_array().unwrap().is_empty());
    assert_eq!(fresh["matched_items"], 0);
    assert_eq!(fresh["items_truncated"], false);
    assert_eq!(fixture.category_report_with_top("PDF", 0, &[]), fresh);
    let text = fixture.output("--report", &["--category", "PDF"]);
    assert_eq!(text, "Largest PDF (0 of 0 shown, 0 B displayed total):\n");

    let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert_eq!(fixture.category_report(&["--cache"]), fresh);
    let cached = fixture.category_report(&["--cache"]);
    let at = assert_u64(&cached, "cached_at");
    let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    assert!((before..=after).contains(&at));
    let mut expected = fresh.clone();
    expected["cached_at"] = at.into();
    assert_eq!(cached, expected, "caching must change only cached_at");
    let full_cached: Value = serde_json::from_str(&fixture.output("--json", &["--cache"])).unwrap();
    assert_eq!(cached["cached_at"], full_cached["cached_at"]);
    assert_eq!(cached["unreadable"], full_cached["unreadable"]);

    let cached_text = fixture.output("--report", &["--category", "PDF", "--cache"]);
    let (notice, body) = cached_text.split_once('\n').unwrap();
    assert!(notice.starts_with("(from cache, scanned "), "{cached_text}");
    assert!(notice.ends_with("; --refresh to rescan)"), "{cached_text}");
    assert_eq!(body, text, "a complete cached scan must not gain an unreadable warning");
    assert_eq!(fixture.category_report(&["--cache"]), cached);
}

#[test]
fn category_limits_count_all_matches_without_changing_tie_order() {
    let fixture = Fixture::new("category-limits");
    for name in ["a.pdf", "z.pdf", "m.pdf"] {
        // Zero-byte files still match, and tie independently of allocation size.
        fs::write(fixture.root.join(name), b"").unwrap();
    }
    fs::write(fixture.root.join("ignored.txt"), b"not a PDF").unwrap();
    let full: Value = serde_json::from_str(&fixture.output_with_top("--json", usize::MAX, &[])).unwrap();
    assert_report_schema(&full, &fixture.root);
    assert_eq!(full["largest_files"].as_array().unwrap().len(), 4);
    for mode in ["du", "dedup"] {
        let all = fixture.category_report(&["--size-mode", mode]);
        let names: Vec<_> = all["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                Path::new(item["path"].as_str().unwrap())
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(names, ["z.pdf", "m.pdf", "a.pdf"]);
        for top in [0, 1, 3, 4, usize::MAX] {
            let report = fixture.category_report_with_top("PDF", top, &["--size-mode", mode]);
            let mut expected = all.clone();
            expected["items"].as_array_mut().unwrap().truncate(top);
            expected["items_truncated"] = (top < 3).into();
            assert_eq!(report, expected, "top={top}, mode={mode}");
            assert_eq!(report["matched_items"], 3);

            let text = fixture.output_with_top("--report", top, &["--category", "PDF", "--size-mode", mode]);
            let suffix = if mode == "dedup" { ", clones counted once" } else { "" };
            assert_eq!(
                text.lines().next().unwrap(),
                format!("Largest PDF ({} of 3 shown, 0 B displayed total{suffix}):", top.min(3))
            );
            assert_eq!(text.lines().count(), top.min(3) + 1);
            for (line, item) in text.lines().skip(1).zip(report["items"].as_array().unwrap()) {
                assert!(line.ends_with(item["path"].as_str().unwrap()), "{line}");
            }
        }
    }
}

#[test]
fn directory_category_counts_prune_matching_subtrees_and_exclude_inherited_files() {
    let fixture = Fixture::new("category-pruning");
    for directory in ["a/node_modules/pkg/node_modules", "b/node_modules", "ordinary"] {
        fs::create_dir_all(fixture.root.join(directory)).unwrap();
        fs::write(fixture.root.join(directory).join("paper.pdf"), b"content").unwrap();
    }
    fs::write(fixture.root.join("visible.pdf"), b"content").unwrap();
    for mode in ["du", "dedup"] {
        let args = ["--size-mode", mode];
        let all = fixture.category_report_with_top("node_modules", usize::MAX, &args);
        assert_eq!(all["matched_items"], 2);
        assert_eq!(all["items_truncated"], false);
        let mut paths: Vec<_> = all["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| PathBuf::from(item["path"].as_str().unwrap()))
            .collect();
        paths.sort();
        let root = fs::canonicalize(&fixture.root).unwrap();
        assert_eq!(paths, [root.join("a/node_modules"), root.join("b/node_modules")]);
        for top in [0, 1, 2, 3] {
            let limited = fixture.category_report_with_top("node_modules", top, &args);
            let mut expected = all.clone();
            expected["items"].as_array_mut().unwrap().truncate(top);
            expected["items_truncated"] = (top < 2).into();
            assert_eq!(limited, expected);
        }
        let files = fixture.category_report_with_top("PDF", usize::MAX, &args);
        assert_eq!(
            files["matched_items"], 2,
            "PDFs inside node_modules inherit its category"
        );
        assert_eq!(files["items_truncated"], false);
        assert_eq!(files["items"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn cli_and_mcp_category_reports_keep_the_same_schema_and_ranked_items() {
    let fixture = Fixture::new("category-schema-parity");
    for (name, size) in [("small.pdf", 1), ("large.pdf", 16384), ("medium.pdf", 8192)] {
        fs::write(fixture.root.join(name), vec![b'x'; size]).unwrap();
    }
    for mode in ["du", "dedup"] {
        let cli = fixture.category_report(&["--size-mode", mode]);
        let full: Value = serde_json::from_str(&fixture.output("--json", &["--size-mode", mode])).unwrap();
        assert_eq!(cli["path"], full["path"]);
        assert_eq!(cli["accounting"], full["accounting"]);
        assert_eq!(cli["unreadable"], full["unreadable"]);
        assert_eq!(cli["items"], full["largest_files"]);
        assert_eq!(cli["items"].as_array().unwrap().len(), 3);
        assert_eq!(cli["matched_items"], 3);
        assert_eq!(cli["items_truncated"], false);
        if mode == "du" {
            let text = fixture.output_with_top("--report", 1, &["--category", "PDF"]);
            assert_eq!(
                text.lines().next().unwrap(),
                format!(
                    "Largest PDF (1 of 3 shown, {} displayed total):",
                    cli["items"][0]["size"].as_str().unwrap()
                )
            );
        }

        let limits = [1, 3, 4, 500];
        let mut child = fixture.command_for("--mcp").stdin(Stdio::piped()).spawn().unwrap();
        let input = limits
            .iter()
            .enumerate()
            .map(|(id, top)| {
                json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{
                "name":"find_by_category", "arguments":{
                    "path":fixture.root, "category":"PDF", "top":top, "size_mode":mode}}})
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let output = bounded_output(child);
        assert!(output.status.success());
        assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
        let responses: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses.len(), limits.len());
        for (response, top) in responses.iter().zip(limits) {
            let result = &response["result"];
            assert_eq!(result["isError"], false);
            let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(text, result["structuredContent"]);
            assert_eq!(
                text,
                fixture.category_report_with_top("PDF", top, &["--size-mode", mode])
            );
            let mut expected = cli.clone();
            expected["items"].as_array_mut().unwrap().truncate(top);
            expected["items_truncated"] = (top < 3).into();
            assert_eq!(text, expected);
        }
    }
    assert!(!fixture.base.join("unused-cache").exists(), "MCP must remain read-only");
}

#[test]
fn equal_sized_entries_keep_stable_json_order() {
    let fixture = Fixture::new("ties");
    for name in ["zeta", "alpha", "middle", "beta"] {
        let directory = fixture.root.join(name);
        fs::create_dir(&directory).unwrap();
        for file in ["z.txt", "a.txt", "m.txt"] {
            fs::write(directory.join(file), b"same sized file\n").unwrap();
        }
    }
    let report = fixture.assert_stable();
    assert_eq!(report["files"], 12);
    assert_eq!(report["unreadable"], 0);
}

#[cfg(unix)]
#[test]
fn static_symlink_loops_are_entries_not_traversals() {
    use std::os::unix::fs::{MetadataExt, symlink};

    let fixture = Fixture::new("symlinks");
    fs::create_dir(fixture.root.join("sub")).unwrap();
    fs::write(fixture.root.join("sub/inside.txt"), b"inside").unwrap();
    fs::create_dir(fixture.base.join("outside")).unwrap();
    fs::write(fixture.base.join("outside/not-in-scan.bin"), [7; 8192]).unwrap();
    for (target, link) in [
        ("self", "self"),
        ("loop-b", "loop-a"),
        ("loop-a", "loop-b"),
        (".", "to-root"),
        ("../outside", "outside"),
        ("missing", "dangling"),
        ("..", "sub/to-parent"),
    ] {
        symlink(target, fixture.root.join(link)).unwrap();
    }
    let mut expected = fs::symlink_metadata(&fixture.root).unwrap().blocks() * 512;
    for directory in [&fixture.root, &fixture.root.join("sub")] {
        for entry in fs::read_dir(directory).unwrap() {
            expected += entry.unwrap().metadata().unwrap().blocks() * 512;
        }
    }
    let report = fixture.assert_stable();
    assert_eq!(report["files"], 8, "seven links plus one regular file");
    assert_eq!(report["unreadable"], 0, "dangling/cyclic links need only lstat");
    assert_eq!(report["size_bytes"], expected, "symlink targets must not be counted");
    for child in report["children"].as_array().unwrap() {
        if child["name"] != "sub" {
            assert_eq!(child["type"], "file", "symlinks must not be descended into");
            assert_eq!(child["files"], 1);
        }
    }
}

#[cfg(unix)]
#[test]
fn symlink_loop_as_root_fails_cleanly_without_json_or_hanging() {
    use std::os::unix::fs::symlink;

    let mut fixture = Fixture::new("symlink-root");
    fixture.root = fixture.base.join("loop");
    symlink("loop", &fixture.root).unwrap();
    let output = bounded_output(fixture.command().spawn().unwrap());
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "an invalid root must not emit a partial JSON document"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("dutype: "), "missing CLI error prefix: {stderr}");
    assert!(
        stderr.contains(fixture.root.to_str().unwrap()),
        "error should identify the invalid root: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "root errors must not panic: {stderr}");
}

#[cfg(unix)]
struct UnreadableDirectory {
    path: PathBuf,
    permissions: fs::Permissions,
}

#[cfg(unix)]
impl UnreadableDirectory {
    fn new(path: &Path) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;

        let guard = Self {
            path: path.to_path_buf(),
            permissions: fs::metadata(path).unwrap().permissions(),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        match fs::read_dir(path) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Some(guard),
            Ok(_) => {
                eprintln!(
                    "SKIP permission-denied assertions: this process can read chmod(000) directories \
                     (for example, privileged CI); fixture={}",
                    path.display()
                );
                None
            }
            Err(error) => panic!("expected PermissionDenied, got {error}"),
        }
    }
}

#[cfg(unix)]
impl Drop for UnreadableDirectory {
    fn drop(&mut self) {
        // Restore permissions even after a failed assertion, before Fixture
        // removes its synthetic tree.
        let _ = fs::set_permissions(&self.path, self.permissions.clone());
    }
}

#[cfg(unix)]
#[test]
fn unreadable_root_is_explicitly_reported_as_partial() {
    let fixture = Fixture::new("denied-root");
    fs::write(fixture.root.join("hidden.txt"), b"must not be read").unwrap();
    let Some(_permissions) = UnreadableDirectory::new(&fixture.root) else {
        return;
    };
    let report = fixture.assert_stable();
    assert_eq!(report["unreadable"], 1);
    assert_eq!(report["files"], 0);
    assert!(report["children"].as_array().unwrap().is_empty());
    let category = fixture.category_report(&[]);
    assert_eq!(category["unreadable"], report["unreadable"]);
    assert!(category["cached_at"].is_null());
    assert!(category["items"].as_array().unwrap().is_empty());
    assert_eq!(
        fixture.output("--report", &["--category", "PDF"]),
        "(1 entries unreadable)\nLargest PDF (0 of 0 shown, 0 B displayed total):\n"
    );
}

#[cfg(unix)]
#[test]
fn unreadable_descendant_does_not_hide_readable_siblings() {
    let fixture = Fixture::new("denied-child");
    let denied = fixture.root.join("denied");
    fs::create_dir(&denied).unwrap();
    fs::write(denied.join("hidden.txt"), b"must not be read").unwrap();
    fs::write(fixture.root.join("visible.txt"), b"still counted").unwrap();
    let Some(_permissions) = UnreadableDirectory::new(&denied) else {
        return;
    };
    let report = fixture.assert_stable();
    assert_eq!(report["unreadable"], 1);
    assert_eq!(report["files"], 1);
    let children = report["children"].as_array().unwrap();
    assert!(children.iter().any(|child| child["name"] == "visible.txt"));
    let denied = children.iter().find(|child| child["name"] == "denied").unwrap();
    assert_eq!(denied["type"], "dir");
    assert_eq!(denied["files"], 0);
    let category = fixture.category_report(&[]);
    assert_eq!(category["unreadable"], report["unreadable"]);
    assert!(category["items"].as_array().unwrap().is_empty());
    assert_eq!(
        fixture.output("--report", &["--category", "PDF"]),
        "(1 entries unreadable)\nLargest PDF (0 of 0 shown, 0 B displayed total):\n"
    );
}

#[cfg(unix)]
#[test]
fn cached_partial_category_stays_partial_after_permissions_recover_until_refresh() {
    let fixture = Fixture::new("cached-partial-category");
    let denied = fixture.root.join("denied");
    fs::create_dir(&denied).unwrap();
    fs::write(denied.join("hidden.pdf"), b"hidden").unwrap();
    fs::write(fixture.root.join("visible.pdf"), b"visible").unwrap();
    let Some(permissions) = UnreadableDirectory::new(&denied) else {
        return;
    };
    let fresh = fixture.category_report(&["--cache"]);
    assert_eq!(fresh["unreadable"], 1);
    assert!(fresh["cached_at"].is_null());
    assert_eq!(fresh["items"].as_array().unwrap().len(), 1);
    assert_eq!(fresh["matched_items"], 1, "unreadable entries are not known matches");
    assert_eq!(fresh["items_truncated"], false);
    assert!(fresh["items"][0]["path"].as_str().unwrap().ends_with("visible.pdf"));
    let cached = fixture.category_report(&["--cache"]);
    assert_u64(&cached, "cached_at");
    assert_eq!(cached["items"], fresh["items"]);

    drop(permissions);
    assert_eq!(fixture.category_report(&["--cache"]), cached);
    let hidden = fixture.category_report_with_top("PDF", 0, &["--cache"]);
    assert_eq!(hidden["matched_items"], 1);
    assert_eq!(hidden["items_truncated"], true);
    assert_eq!(hidden["items"], json!([]));
    assert_eq!(hidden["cached_at"], cached["cached_at"]);
    let full: Value = serde_json::from_str(&fixture.output("--json", &["--cache"])).unwrap();
    assert_eq!(full["unreadable"], cached["unreadable"]);
    assert_eq!(full["cached_at"], cached["cached_at"]);
    assert_eq!(full["largest_files"], cached["items"]);
    let text = fixture.output("--report", &["--category", "PDF", "--cache"]);
    let mut lines = text.lines();
    let notice = lines.next().unwrap();
    assert!(notice.starts_with("(from cache, scanned "), "{text}");
    assert!(notice.ends_with("; --refresh to rescan)"), "{text}");
    assert_eq!(lines.next(), Some("(1 entries unreadable)"));
    assert!(lines.next().unwrap().starts_with("Largest PDF (1 of 1 shown, "));

    let refreshed = fixture.category_report(&["--refresh"]);
    assert_eq!(refreshed["unreadable"], 0);
    assert!(refreshed["cached_at"].is_null());
    assert_eq!(refreshed["items"].as_array().unwrap().len(), 2);
    assert_eq!(refreshed["matched_items"], 2);
    assert_eq!(refreshed["items_truncated"], false);
    let limited = fixture.category_report_with_top("PDF", 1, &["--cache"]);
    assert_eq!(limited["matched_items"], 2);
    assert_eq!(limited["items_truncated"], true);
    let cached_refreshed = fixture.category_report(&["--cache"]);
    assert_u64(&cached_refreshed, "cached_at");
    assert_eq!(cached_refreshed["unreadable"], 0);
    assert_eq!(cached_refreshed["items"], refreshed["items"]);
    let text = fixture.output("--report", &["--category", "PDF", "--refresh"]);
    assert!(text.starts_with("Largest PDF (2 of 2 shown, "));
    assert!(!text.contains("entries unreadable") && !text.contains("from cache"));
}

struct FileChurn {
    stop: Arc<AtomicBool>,
    removed: Arc<AtomicU64>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FileChurn {
    fn start(paths: Vec<PathBuf>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let removed = Arc::new(AtomicU64::new(0));
        let (worker_stop, worker_removed) = (stop.clone(), removed.clone());
        let thread = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                for path in &paths {
                    fs::remove_file(path).unwrap();
                    worker_removed.fetch_add(1, Ordering::Relaxed);
                }
                for path in &paths {
                    fs::write(path, b"temporary").unwrap();
                }
            }
        });
        Self {
            stop,
            removed,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.thread.take() {
            worker.join().unwrap();
        }
    }
}

impl Drop for FileChurn {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn bounded_stress_files_disappearing_during_scan_keep_valid_json() {
    // This is deliberately a bounded stress test, not a claim to force a
    // particular readdir/lstat interleaving. Counts during churn may vary,
    // including unreadable=0 when no metadata lookup loses the race. There is
    // intentionally no upper file-count assertion: readdir may yield a name
    // again when the directory is concurrently modified.
    const FILES: u64 = 1024;
    let fixture = Fixture::new("disappearing");
    let volatile = fixture.root.join("volatile");
    fs::create_dir(&volatile).unwrap();
    fs::write(fixture.root.join("permanent.txt"), b"always present").unwrap();
    let paths: Vec<_> = (0..FILES)
        .map(|i| volatile.join(format!("vanishing-{i:04}.txt")))
        .collect();
    for path in &paths {
        fs::write(path, b"temporary").unwrap();
    }
    let mut churn = FileChurn::start(paths);
    for _ in 0..4 {
        let report = fixture.report();
        let files = report["files"].as_u64().unwrap();
        assert!(files >= 1, "the unchanged sibling must still be counted");
        assert!(
            report["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|child| { child["name"] == "permanent.txt" && child["files"] == 1 })
        );
    }
    churn.stop();
    assert!(
        churn.removed.load(Ordering::Relaxed) > 0,
        "stress worker did not remove any files"
    );
    // Once mutations stop, the final full pass must neither retain stale
    // errors nor omit files, and its public JSON must be repeatable again.
    let report = fixture.assert_stable();
    assert_eq!(report["files"], FILES + 1);
    assert_eq!(report["unreadable"], 0);
}

#[test]
fn relative_root_reports_absolute_paths() {
    let fixture = Fixture::new("relative-root");
    fs::write(fixture.root.join("paper.pdf"), b"content").unwrap();
    let output = bounded_output(
        Command::new(env!("CARGO_BIN_EXE_dutype"))
            .args(["--json", "--config"])
            .arg(&fixture.config)
            .arg("tree")
            .current_dir(&fixture.base)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_report_schema(&report, &fixture.root);
    assert_eq!(report["largest_files"].as_array().unwrap().len(), 1);
}
