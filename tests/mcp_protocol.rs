use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "dutype-mcp-protocol-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("config.toml"), "").unwrap();
        Self(path)
    }

    fn run(&self, input: &str) -> Vec<Value> {
        self.run_with_args(input, &[])
    }

    fn run_with_args(&self, input: &str, extra_args: &[&str]) -> Vec<Value> {
        let stdout = self.0.join("stdout.jsonl");
        let stderr = self.0.join("stderr.txt");
        let mut child = Command::new(env!("CARGO_BIN_EXE_dutype"))
            .args(["--mcp", "--config"])
            .arg(self.0.join("config.toml"))
            .args(extra_args)
            .current_dir(&self.0)
            .env("DUTYPE_CACHE_DIR", self.0.join("cache"))
            .env("RAYON_NUM_THREADS", "2")
            .stdin(Stdio::piped())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("MCP server timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let errors = fs::read_to_string(stderr).unwrap();
        assert!(status.success(), "{errors}");
        assert!(errors.is_empty(), "{errors}");
        fs::read_to_string(stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("stdout must contain only one JSON-RPC object per line"))
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn stdio_recovers_from_malformed_messages_without_losing_following_requests() {
    let fixture = Fixture::new();
    let responses = fixture.run(
        "\n{not json}\nnull\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
         {\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n",
    );
    assert_eq!(responses.len(), 3, "{responses:?}");
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(responses[0]["id"], Value::Null);
    assert_eq!(responses[1]["error"]["code"], -32600);
    assert_eq!(responses[1]["id"], Value::Null);
    assert_eq!(responses[2], json!({"jsonrpc": "2.0", "id": 7, "result": {}}));
}

#[test]
fn tool_text_and_structured_content_agree_and_execution_errors_stay_results() {
    let fixture = Fixture::new();
    // Unicode and spaces are portable; the Unix filename also checks that
    // embedded newlines and quotes cannot break the stdio message boundary.
    let name = if cfg!(unix) {
        "空目錄 \"quoted\"\nline"
    } else {
        "空目錄 spaced"
    };
    let directory = fixture.0.join(name);
    fs::create_dir(&directory).unwrap();
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "dutype-regression-test", "version": "1"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "disk_usage", "arguments": {"path": directory}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "find_by_category", "arguments": {"path": directory, "category": "PDF"}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "disk_usage", "arguments": {"path": directory.join("missing")}}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let responses = fixture.run(&input);
    assert_eq!(responses.len(), 5, "{responses:?}");
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(responses[1]["result"]["tools"].as_array().unwrap().len(), 2);
    for response in &responses[2..4] {
        let result = &response["result"];
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
        let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"]);
        assert_eq!(
            text["path"],
            fs::canonicalize(&directory).unwrap().display().to_string()
        );
        assert!(text["cached_at"].is_null());
        assert!(text["accounting"].is_object());
    }
    let report = &responses[2]["result"]["structuredContent"];
    assert!(report["size_bytes"].is_u64());
    assert_eq!(report["children"], json!([]));
    assert_eq!(responses[3]["result"]["structuredContent"]["items"], json!([]));
    assert_eq!(responses[4]["result"]["isError"], true);
    assert!(responses[4].get("error").is_none());
    assert!(responses[4]["result"].get("structuredContent").is_none());
}

#[cfg(unix)]
#[test]
fn deep_scans_survive_repeated_requests_cache_replacement_and_shutdown() {
    struct Tree(Vec<PathBuf>);
    impl Drop for Tree {
        fn drop(&mut self) {
            for path in self.0.iter().rev() {
                let _ = fs::remove_dir(path);
            }
        }
    }

    let fixture = Fixture::new();
    let directory = fixture.0.join("deep");
    fs::create_dir(&directory).unwrap();
    let mut tree = Tree(vec![directory.clone()]);
    let mut path = directory.clone();
    // Linux permits a path deep enough to exhaust a normal worker's stack;
    // macOS has a smaller PATH_MAX, so keep its fixture below that limit.
    let depth = if cfg!(target_os = "linux") { 1500 } else { 400 };
    for _ in 0..depth {
        path.push("d");
        fs::create_dir(&path).unwrap();
        tree.0.push(path.clone());
    }
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "disk_usage", "arguments": {"path": directory, "top": 1}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "find_by_category", "arguments": {"path": directory, "category": "PDF"}}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "disk_usage", "arguments": {"path": directory, "top": 1, "refresh": true}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "disk_usage", "arguments": {"path": directory, "top": 1}}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    for args in [&[][..], &["--cache"][..]] {
        let responses = fixture.run_with_args(&input, args);
        assert_eq!(responses.len(), 4);
        for response in &responses {
            assert_eq!(response["result"]["isError"], false, "{response}");
        }
        assert_eq!(responses[0]["result"]["structuredContent"]["files"], 0);
        assert_eq!(responses[0]["result"]["structuredContent"]["unreadable"], 0);
        assert_eq!(responses[1]["result"]["structuredContent"]["items"], json!([]));
        assert_eq!(
            responses[3]["result"]["structuredContent"]["cached_at"].is_u64(),
            !args.is_empty()
        );
        // MCP tools must remain read-only even when caching scans in memory.
        assert!(!fixture.0.join("cache").exists());
    }
}

#[test]
fn relative_tool_paths_are_absolute_in_both_payloads() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("tree")).unwrap();
    fs::write(fixture.0.join("tree/paper.pdf"), b"pdf").unwrap();
    let input = [
        json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"disk_usage", "arguments":{"path":"tree"}}}),
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"find_by_category", "arguments":{"path":"tree", "category":"PDF"}}}),
    ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
    let responses = fixture.run(&input);
    assert_eq!(responses.len(), 2);
    for response in &responses {
        let result = &response["result"];
        assert_eq!(result["isError"], false);
        let payload = &result["structuredContent"];
        assert_eq!(
            serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
            *payload
        );
        assert!(std::path::Path::new(payload["path"].as_str().unwrap()).is_absolute());
        for key in ["children", "largest_files", "items"] {
            if let Some(items) = payload[key].as_array() {
                for item in items {
                    assert!(std::path::Path::new(item["path"].as_str().unwrap()).is_absolute());
                }
            }
        }
    }
}
