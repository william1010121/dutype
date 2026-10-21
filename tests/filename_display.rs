//! Human output and machine paths are separate contracts. Capture all output as
//! bytes, never drive a real terminal or invoke a reveal/clipboard program.
#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    base: PathBuf,
    root: PathBuf,
    config: PathBuf,
}
impl Fixture {
    fn new(root: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "dutype-display-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join(root);
        fs::create_dir(&root).unwrap();
        // CLI/MCP paths are canonical, including macOS /var -> /private/var.
        let root = fs::canonicalize(root).unwrap();
        let config = base.join("config.toml");
        fs::write(&config, "").unwrap();
        Self { base, root, config }
    }
    fn run(&self, args: &[&str], path: Option<&Path>, input: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dutype"));
        command
            .args(args)
            .args(["--config"])
            .arg(&self.config)
            .current_dir(&self.base)
            .env("DUTYPE_CACHE_DIR", self.base.join("cache"))
            .env("RAYON_NUM_THREADS", "2");
        if let Some(path) = path {
            command.arg("--").arg(path);
        }
        let stdin = self.base.join("stdin");
        fs::write(&stdin, input.unwrap_or("")).unwrap();
        let stdout = self.base.join("stdout");
        let stderr = self.base.join("stderr");
        command
            .stdin(fs::File::open(stdin).unwrap())
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("filename fixture timed out");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        }
    }
    fn success(&self, args: &[&str]) -> Vec<u8> {
        let output = self.run(args, Some(&self.root), None);
        assert!(output.status.success() && output.stderr.is_empty(), "{output:?}");
        output.stdout
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn report_controls_are_visible_while_json_mcp_and_cache_keep_original_paths() {
    let fixture = Fixture::new("root\n");
    let names = [
        "a\nb.txt",
        "a\\nb.txt",
        "ab.txt",
        "a\tb.txt",
        "a\rb.txt",
        "a\x07b.txt",
        "a\x1bb.txt",
        "a\u{85}b.txt",
        "a\u{202e}b.txt",
        "a\u{2066}b.txt",
        "a\u{2028}b.txt",
        "résumé-👩\u{200d}💻.txt",
        "-leading.txt",
    ];
    for name in names {
        fs::write(fixture.root.join(name), vec![0; 4096]).unwrap();
    }
    let raw: Value = serde_json::from_slice(&fixture.success(&["--json", "--cache", "--top", "100"])).unwrap();
    let mut cached: Value = serde_json::from_slice(&fixture.success(&["--json", "--cache", "--top", "100"])).unwrap();
    let mut original = raw.clone();
    original.as_object_mut().unwrap().remove("cached_at");
    cached.as_object_mut().unwrap().remove("cached_at");
    assert_eq!(original, cached);
    for name in names {
        assert!(
            raw["children"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == name && c["path"] == fixture.root.join(name).to_str().unwrap())
        );
    }
    let text = String::from_utf8(fixture.success(&["--report", "--top", "100"])).unwrap();
    assert!(text.contains("root\\n"));
    assert!(text.contains("a\\nb.txt"));
    assert!(text.contains("a\\\\nb.txt"));
    assert!(text.contains("a\\u{0085}b.txt"));
    assert!(text.contains("a\\u{202e}b.txt"));
    assert!(text.contains("a\\u{2066}b.txt"));
    assert!(text.contains("a\\u{2028}b.txt"));
    assert!(text.contains("résumé-👩\u{200d}💻.txt"));
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    let category = raw["children"][0]["top_categories"][0]["name"].as_str().unwrap();
    let category_text =
        String::from_utf8(fixture.success(&["--report", "--category", category, "--top", "100"])).unwrap();
    assert_eq!(category_text.lines().count(), names.len() + 1);
    let request = format!(
        "{}\n",
        json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"disk_usage", "arguments":{"path": fixture.root, "top":100}}})
    );
    let output = fixture.run(&["--mcp"], None, Some(&request));
    assert!(output.status.success() && output.stderr.is_empty(), "{output:?}");
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    let report = &reply["result"]["structuredContent"];
    assert_eq!(report["path"], raw["path"]);
    assert_eq!(report["children"], raw["children"]);
    assert_eq!(report["largest_files"], raw["largest_files"]);
    let content: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&content, report);
}

#[test]
fn leading_dash_paths_and_human_error_prefixes_keep_their_roles() {
    let fixture = Fixture::new("-leading");
    fs::write(fixture.root.join("-name.txt"), b"data").unwrap();
    let output = fixture.run(&["--json"], Some(Path::new("-leading")), None);
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["path"], fixture.root.to_str().unwrap());
    let output = fixture.run(&["--report"], Some(Path::new("missing\n\x1b\u{202e}")), None);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.starts_with(b"dutype: missing\\n\\x1b\\u{202e}: "));
    assert!(!output.stderr.contains(&0x1b));
    assert_eq!(output.stderr.iter().filter(|&&b| b == b'\n').count(), 1);
}

// Some Unix filesystems (notably APFS) reject invalid UTF-8 filenames. This
// guard applies only to the first invalid-name creation, never later operations.
fn invalid_name_fixture_supported(result: std::io::Result<()>) -> std::io::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => Ok(false),
        Err(error) => Err(error),
    }
}

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

#[test]
fn existing_non_utf8_display_aliases_are_not_claimed_to_be_unique() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let fixture = Fixture::new("lossy");
    if !invalid_name_fixture_supported(fs::write(fixture.root.join(OsStr::from_bytes(b"bad\xff\n.txt")), b"a")).unwrap()
    {
        eprintln!("skipping invalid-name fixture: this filesystem rejects non-UTF-8 names (EILSEQ)");
        return;
    }
    fs::write(fixture.root.join("bad\\xff\n.txt"), b"b").unwrap();
    let raw: Value = serde_json::from_slice(&fixture.success(&["--json"])).unwrap();
    assert_eq!(raw["children"].as_array().unwrap().len(), 2);
    assert!(
        raw["children"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] == "bad\\xff\n.txt")
    );
    let text = String::from_utf8(fixture.success(&["--report"])).unwrap();
    assert_eq!(text.matches("bad\\\\xff\\n.txt").count(), 4);
}
