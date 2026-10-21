//! Config discovery is checked in isolated subprocesses so environment variables
//! and the process-global registry cannot leak between cases. Fixtures are shallow.

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
const MODES: [&str; 3] = ["--json", "--report", "--mcp"];

#[derive(Clone, Copy, Debug)]
enum Source {
    Default,
    Environment,
    Explicit,
}

const SOURCES: [Source; 3] = [Source::Default, Source::Environment, Source::Explicit];

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    config: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "dutype-config-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join("tree");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("sample.fixtureconfig"), vec![b'x'; 8192]).unwrap();
        Self {
            root,
            config: base.join("config-home").join("dutype").join("config.toml"),
            cache: base.join("cache"),
            base,
        }
    }

    fn write_config(&self, contents: impl AsRef<[u8]>) {
        fs::create_dir_all(self.config.parent().unwrap()).unwrap();
        fs::write(&self.config, contents).unwrap();
    }

    fn command(&self, mode: &str, source: Source, root: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dutype"));
        command
            .args([mode, "--cache"])
            .current_dir(&self.base)
            .env_remove("DUTYPE_CONFIG")
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config-home"))
            .env("APPDATA", self.base.join("config-home"))
            .env("DUTYPE_CACHE_DIR", &self.cache)
            .env("RAYON_NUM_THREADS", "2");
        match source {
            Source::Default => {}
            Source::Environment => {
                command.env("DUTYPE_CONFIG", &self.config);
            }
            Source::Explicit => {
                command.arg("--config").arg(&self.config);
            }
        }
        if mode != "--mcp" {
            command.arg(root);
        }
        let input = self.base.join("input.jsonl");
        fs::write(
            &input,
            format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "disk_usage", "arguments": {"path": root}}})
            ),
        )
        .unwrap();
        command.stdin(fs::File::open(input).unwrap());
        command
    }

    fn run(&self, mut command: Command) -> Output {
        // File-backed output cannot fill a pipe while waiting for the child.
        let stdout = self.base.join("stdout");
        let stderr = self.base.join("stderr");
        command
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("config subprocess exceeded its deadline: {command:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        }
    }

    fn assert_success(&self, source: Source, category: &str) {
        for mode in MODES {
            let output = self.run(self.command(mode, source, &self.root));
            if let Some(report) = successful_report(mode, &output) {
                assert_category(&report, category);
            } else {
                assert!(String::from_utf8_lossy(&output.stdout).contains(category));
            }
        }
    }

    fn assert_failure(&self, source: Source) {
        for mode in MODES {
            // The config error must win even before target resolution, not merely
            // before reporting. MCP must not reply to the queued scan request.
            let missing_root = self.base.join("missing-scan-root");
            let output = self.run(self.command(mode, source, &missing_root));
            assert_eq!(output.status.code(), Some(1), "{source:?} {mode}: {output:?}");
            assert!(output.stdout.is_empty(), "{source:?} {mode}: {output:?}");
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(
                error.starts_with(&format!("dutype: {}: ", self.config.display())),
                "{source:?} {mode}: {error}"
            );
            assert!(!error.contains("missing-scan-root"), "{error}");
        }
    }

    fn json(&self) -> Value {
        successful_report("--json", &self.run(self.command("--json", Source::Default, &self.root))).unwrap()
    }

    fn cache_contents(&self) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        let mut contents: Vec<_> = fs::read_dir(&self.cache)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = fs::read(&path).unwrap();
                let modified = fs::metadata(&path).unwrap().modified().unwrap();
                (path, bytes, modified)
            })
            .collect();
        contents.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(contents.len(), 1, "only one shallow scan should be cached");
        contents
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn config(category: &str) -> String {
    format!("icons = \"none\"\n[[category]]\nname = \"{category}\"\nextensions = [\"fixtureconfig\"]\n")
}

fn successful_report(mode: &str, output: &Output) -> Option<Value> {
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "{mode}: {output:?}"
    );
    assert!(!output.stdout.contains(&0x1b), "terminal escape in {mode} output");
    if mode == "--report" {
        assert!(!output.stdout.is_empty());
        return None;
    }
    // Parsing the whole buffer also rejects banners or extra JSON/RPC output.
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    if mode == "--json" {
        return Some(value);
    }
    assert_eq!(value["id"], 1);
    assert!(value.get("error").is_none(), "{value}");
    assert_eq!(value["result"]["isError"], false);
    assert_eq!(value["result"]["content"].as_array().unwrap().len(), 1);
    let text: Value = serde_json::from_str(value["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, value["result"]["structuredContent"]);
    Some(text)
}

fn assert_category(report: &Value, name: &str) {
    assert_eq!(report["files"], 1);
    assert!(
        report["categories"].as_array().unwrap().iter().any(|category| {
            category["name"] == name && category["size_bytes"].as_u64().is_some_and(|size| size > 0)
        }),
        "missing category {name}: {report}"
    );
}

#[test]
fn missing_implicit_configs_are_optional_and_are_not_created() {
    for source in [Source::Default, Source::Environment] {
        let fixture = Fixture::new();
        fixture.assert_success(source, "Other");
        assert_eq!(
            fs::metadata(&fixture.config).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(!fixture.config.parent().unwrap().exists());
    }
}

#[test]
fn missing_explicit_config_fails_before_scanning_or_creating_cache() {
    let fixture = Fixture::new();
    fixture.assert_failure(Source::Explicit);
    assert!(!fixture.cache.exists());
}

#[test]
fn valid_configs_work_in_every_mode_and_source() {
    for source in SOURCES {
        let fixture = Fixture::new();
        fixture.write_config(config("Project data"));
        fixture.assert_success(source, "Project data");
    }
}

#[test]
fn explicit_then_environment_config_precedence_is_preserved() {
    let fixture = Fixture::new();
    fixture.write_config("not valid TOML");
    let environment = fixture.base.join("environment.toml");
    let explicit = fixture.base.join("explicit.toml");
    fs::write(&environment, config("Environment rules")).unwrap();
    fs::write(&explicit, config("Explicit rules")).unwrap();
    for mode in MODES {
        for (path, expected) in [(None, "Environment rules"), (Some(&explicit), "Explicit rules")] {
            let mut command = fixture.command(mode, Source::Default, &fixture.root);
            command.env("DUTYPE_CONFIG", &environment);
            if let Some(path) = path {
                command.arg("--config").arg(path);
            }
            let output = fixture.run(command);
            if let Some(report) = successful_report(mode, &output) {
                assert_category(&report, expected);
            } else {
                assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
            }
        }
    }
    // An absent environment override does not fall through to the invalid
    // platform default; it retains its existing optional-config semantics.
    for mode in MODES {
        let mut command = fixture.command(mode, Source::Default, &fixture.root);
        command.env("DUTYPE_CONFIG", fixture.base.join("missing-environment.toml"));
        let output = fixture.run(command);
        if let Some(report) = successful_report(mode, &output) {
            assert_category(&report, "Other");
        }
    }
}

#[cfg(not(windows))]
#[test]
fn xdg_precedes_home_and_unset_xdg_uses_home() {
    let fixture = Fixture::new();
    fixture.write_config(config("XDG rules"));
    let home_config = fixture.base.join("home/.config/dutype/config.toml");
    fs::create_dir_all(home_config.parent().unwrap()).unwrap();
    fs::write(home_config, config("Home rules")).unwrap();
    assert_category(&fixture.json(), "XDG rules");
    let mut command = fixture.command("--json", Source::Default, &fixture.root);
    command.env_remove("XDG_CONFIG_HOME");
    let output = fixture.run(command);
    assert_category(&successful_report("--json", &output).unwrap(), "Home rules");
}

#[test]
fn unavailable_default_location_uses_builtin_rules() {
    let fixture = Fixture::new();
    fixture.write_config("not valid TOML");
    let mut command = fixture.command("--json", Source::Default, &fixture.root);
    command
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("APPDATA");
    let output = fixture.run(command);
    assert_category(&successful_report("--json", &output).unwrap(), "Other");
}

#[test]
fn invalid_contents_and_non_file_configs_fail_without_cache_artifacts() {
    for source in SOURCES {
        let fixture = Fixture::new();
        for contents in [b"not valid TOML".as_slice(), b"\xff\xfe".as_slice()] {
            fixture.write_config(contents);
            fixture.assert_failure(source);
            assert!(!fixture.cache.exists());
        }
        fs::remove_file(&fixture.config).unwrap();
        fs::create_dir(&fixture.config).unwrap();
        fixture.assert_failure(source);
        assert!(!fixture.cache.exists());
    }
}

#[test]
fn corrected_config_invalidates_old_cache_and_then_reuses_new_cache() {
    let fixture = Fixture::new();
    fixture.write_config(config("Original rules"));
    assert!(fixture.json()["cached_at"].is_null());
    let warm = fixture.json();
    assert!(warm["cached_at"].is_u64());
    assert_category(&warm, "Original rules");
    let before = fixture.cache_contents();
    fixture.write_config("not valid TOML");
    for source in SOURCES {
        fixture.assert_failure(source);
    }
    assert_eq!(fixture.cache_contents(), before);
    fixture.write_config(config("Corrected rules"));
    let corrected = fixture.json();
    assert!(corrected["cached_at"].is_null());
    assert_category(&corrected, "Corrected rules");
    let warm = fixture.json();
    assert!(warm["cached_at"].is_u64());
    assert_category(&warm, "Corrected rules");
    let after = fixture.cache_contents();
    fixture.assert_success(Source::Default, "Corrected rules");
    assert_eq!(fixture.cache_contents(), after);
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct RestorePermissions {
        path: PathBuf,
        permissions: fs::Permissions,
    }

    impl RestorePermissions {
        fn deny(path: &Path) -> Self {
            let restore = Self {
                path: path.to_path_buf(),
                permissions: fs::metadata(path).unwrap().permissions(),
            };
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
            restore
        }
    }

    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            fs::set_permissions(&self.path, self.permissions.clone()).unwrap();
        }
    }

    fn permission_denied(path: &Path) -> bool {
        match fs::read_to_string(path) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => true,
            Ok(_) => {
                eprintln!(
                    "skipping permission-denied case: this process can read {} despite mode 000",
                    path.display()
                );
                false
            }
            Err(error) => panic!(
                "permission fixture returned an unexpected error for {}: {error}",
                path.display()
            ),
        }
    }

    #[test]
    fn dangling_implicit_configs_are_optional_but_explicit_is_strict() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.config.parent().unwrap()).unwrap();
        symlink(fixture.base.join("missing-target"), &fixture.config).unwrap();
        for source in [Source::Default, Source::Environment] {
            fixture.assert_success(source, "Other");
        }
        fixture.assert_failure(Source::Explicit);
        assert!(fs::symlink_metadata(&fixture.config).unwrap().file_type().is_symlink());
        assert!(!fixture.base.join("missing-target").exists());
    }

    #[test]
    fn non_directory_parent_is_not_silently_treated_as_missing() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.base.join("config-home")).unwrap();
        fs::write(fixture.config.parent().unwrap(), "not a directory").unwrap();
        for source in SOURCES {
            fixture.assert_failure(source);
        }
        assert!(!fixture.cache.exists());
    }

    #[test]
    fn unreadable_config_file_and_parent_are_not_optional() {
        for parent in [false, true] {
            let fixture = Fixture::new();
            fixture.write_config(config("Project data"));
            let path = if parent {
                fixture.config.parent().unwrap()
            } else {
                &fixture.config
            };
            {
                let _restore = RestorePermissions::deny(path);
                if !permission_denied(&fixture.config) {
                    continue;
                }
                for source in SOURCES {
                    fixture.assert_failure(source);
                }
                assert!(!fixture.cache.exists());
            }
            fixture.assert_success(Source::Default, "Project data");
        }
    }

    #[test]
    fn unreadable_parent_preserves_cache_and_recovers_after_permissions_are_restored() {
        let fixture = Fixture::new();
        fixture.write_config(config("Project data"));
        assert_category(&fixture.json(), "Project data");
        assert!(fixture.json()["cached_at"].is_u64());
        let before = fixture.cache_contents();
        {
            let _restore = RestorePermissions::deny(fixture.config.parent().unwrap());
            if !permission_denied(&fixture.config) {
                return;
            }
            for source in SOURCES {
                fixture.assert_failure(source);
            }
            assert_eq!(fixture.cache_contents(), before);
        }
        fixture.assert_success(Source::Default, "Project data");
        assert!(fixture.json()["cached_at"].is_u64());
        assert_eq!(fixture.cache_contents(), before);
    }
}
