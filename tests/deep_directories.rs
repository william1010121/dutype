//! A long, narrow tree must not exhaust a scanner thread's stack.
#[cfg(unix)]
#[test]
fn deep_directory_json_finishes_without_stack_overflow() {
    use std::{
        fs,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    let root = std::env::temp_dir().join(format!("diskbytype-deep-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    struct Tree(Vec<std::path::PathBuf>);
    impl Drop for Tree {
        fn drop(&mut self) {
            for path in self.0.iter().rev() {
                let _ = fs::remove_dir(path);
            }
        }
    }
    let mut tree = Tree(vec![root.clone()]);
    let mut path = root.clone();
    // Below the usual Unix PATH_MAX, but enough to overflow the old recursive scan.
    let depth = if cfg!(target_os = "linux") { 1500 } else { 400 };
    for _ in 0..depth {
        path.push("d");
        fs::create_dir(&path).unwrap();
        tree.0.push(path.clone());
    }
    let config = root.with_extension("toml");
    fs::write(&config, "").unwrap();
    let output = root.with_extension("json");
    let cache = root.with_extension("cache");
    // Cold then repeated cache-enabled scans exercise full snapshot writing
    // and loading. The final request must be a cache hit, not a silent rescan.
    for (use_cache, expect_hit) in [(false, false), (true, false), (true, true)] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_diskbytype"));
        if use_cache {
            command.arg("--cache");
        }
        let mut child = command
            .env("DUTYPE_CACHE_DIR", &cache)
            .args(["--json", "--top", "1", "--config"])
            .arg(&config)
            .arg(&root)
            .env("RAYON_NUM_THREADS", "2")
            .stdout(fs::File::create(&output).unwrap())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("deep scan timed out");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let result = child.wait_with_output().unwrap();
        let bytes = fs::read(&output).unwrap();
        fs::remove_file(&output).unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["cached_at"].is_u64(), expect_hit);
        assert_eq!(json["files"], 0);
        assert_eq!(json["unreadable"], 0);
        assert_eq!(json["children"][0]["name"], "d");
    }
    fs::remove_dir_all(cache).unwrap();
    fs::remove_file(config).unwrap();
}
