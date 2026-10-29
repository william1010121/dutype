//! Non-interactive output must tolerate consumers closing their pipe early.
#[test]
fn reports_exit_cleanly_when_stdout_is_closed() {
    use std::{
        fs,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    let dir = std::env::temp_dir().join(format!("diskbytype-closed-pipe-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    // More than a pipe buffer, even if the child wins the scheduling race.
    for i in 0..500 {
        fs::write(dir.join(format!("file-{i:03}-{}.rs", "x".repeat(100))), b"x").unwrap();
    }
    let config = dir.with_extension("toml");
    fs::write(&config, "").unwrap();
    for mode in ["--json", "--report"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_diskbytype"))
            .args([mode, "--top", "500", "--config"])
            .arg(&config)
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_dir_all(&dir);
                panic!("{mode} did not exit after stdout closed");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stderr.is_empty());
    }
    fs::remove_dir_all(dir).unwrap();
    fs::remove_file(config).unwrap();
}
