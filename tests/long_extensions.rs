//! Long custom suffixes, mixed inventory, and same-version cache compatibility.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const LONG: &str = "abcdefghijklmnopq";
const WITH_RULE: &str = "[[category]]\nname = \"Long\"\ndirs = [\"bundle\"]\nextensions = [\"abcdefghijklmnopq\"]\n";
const WITHOUT_RULE: &str = "[[category]]\nname = \"Long\"\ndirs = [\"bundle\"]\n";
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(config: &str) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!(
            "diskbytype-long-extensions-{}-{nonce}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("tree");
        fs::create_dir_all(&root).unwrap();
        let config_path = base.join("rules.toml");
        fs::write(&config_path, config).unwrap();
        Self {
            base,
            root,
            config: config_path,
        }
    }

    fn run(&self, options: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_diskbytype"))
            .arg("--config")
            .arg(&self.config)
            .arg("--json")
            .args(options)
            .arg(&self.root)
            .env("DUTYPE_CACHE_DIR", self.base.join("cache"))
            .env("RAYON_NUM_THREADS", "2")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(report.is_object());
        assert!(report["accounting"].is_object());
        let cached_at = report.get("cached_at").expect("cache status field");
        assert!(cached_at.is_null() || cached_at.is_u64());
        report
    }

    fn cache_file(&self) -> PathBuf {
        let files: Vec<_> = fs::read_dir(self.base.join("cache"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        files[0].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

// The old fingerprint hashes package version and the exact source texts only.
// This is deliberately independent of the candidate's classification revision.
fn legacy_fingerprint(user: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for source in [
        env!("CARGO_PKG_VERSION"),
        include_str!("../src/default_config.toml"),
        user,
    ] {
        for byte in source.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

fn only_file_category(report: &Value) -> &str {
    let files = report["largest_files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    files[0]["category"].as_str().unwrap()
}

#[test]
fn same_version_legacy_other_cache_is_rebuilt_then_reused() {
    // Both configs create identical categories; only filename attribution differs.
    let fixture = Fixture::new(WITHOUT_RULE);
    fs::write(fixture.root.join(format!("export.{LONG}")), b"payload").unwrap();
    let stale = fixture.run(&["--cache"]);
    assert!(stale["cached_at"].is_null());
    assert_eq!(only_file_category(&stale), "Other");
    let cache = fixture.cache_file();
    let mut bytes = fs::read(&cache).unwrap();
    assert_eq!(&bytes[..8], b"DUTYPE\0\x09");
    assert_eq!(&bytes[8..16], &legacy_fingerprint(WITHOUT_RULE).to_le_bytes());

    // Model an old same-package-version writer: stale Other tree plus the old
    // long-config fingerprint. No historical executable or codec edits needed.
    bytes[8..16].copy_from_slice(&legacy_fingerprint(WITH_RULE).to_le_bytes());
    fs::write(&cache, &bytes).unwrap();
    fs::write(&fixture.config, WITH_RULE).unwrap();
    let rebuilt = fixture.run(&["--cache"]);
    assert!(rebuilt["cached_at"].is_null(), "legacy snapshot must not be accepted");
    assert_eq!(only_file_category(&rebuilt), "Long");
    assert_eq!(
        rebuilt["size_bytes"].as_u64().expect("rebuilt byte total"),
        stale["size_bytes"].as_u64().expect("stale byte total")
    );
    let rewritten = fs::read(&cache).unwrap();
    assert_ne!(&rewritten[8..16], &legacy_fingerprint(WITH_RULE).to_le_bytes());
    let cached = fixture.run(&["--cache"]);
    assert!(cached["cached_at"].is_u64());
    assert_eq!(only_file_category(&cached), "Long");
}

#[test]
fn no_long_rules_keep_legacy_cache_and_rule_add_remove_invalidates() {
    let fixture = Fixture::new(WITHOUT_RULE);
    fs::write(fixture.root.join(format!("export.{LONG}")), b"payload").unwrap();
    let cold = fixture.run(&["--cache"]);
    assert!(cold["cached_at"].is_null());
    let bytes = fs::read(fixture.cache_file()).unwrap();
    assert_eq!(&bytes[8..16], &legacy_fingerprint(WITHOUT_RULE).to_le_bytes());
    assert!(fixture.run(&["--cache"])["cached_at"].is_u64());

    fs::write(&fixture.config, WITH_RULE).unwrap();
    let added = fixture.run(&["--cache"]);
    assert!(added["cached_at"].is_null());
    assert_eq!(only_file_category(&added), "Long");
    assert!(fixture.run(&["--cache"])["cached_at"].is_u64());

    fs::write(&fixture.config, WITHOUT_RULE).unwrap();
    let removed = fixture.run(&["--cache"]);
    assert!(removed["cached_at"].is_null());
    assert_eq!(only_file_category(&removed), "Other");
    let bytes = fs::read(fixture.cache_file()).unwrap();
    assert_eq!(&bytes[8..16], &legacy_fingerprint(WITHOUT_RULE).to_le_bytes());
    assert!(fixture.run(&["--cache"])["cached_at"].is_u64());
}

#[test]
fn mixed_inventory_uses_active_long_rules_and_preserves_directory_precedence() {
    let fixture = Fixture::new(WITH_RULE);
    let bundle = fixture.root.join("bundle");
    fs::create_dir(&bundle).unwrap();
    fs::write(bundle.join(format!("pruned.{LONG}")), b"inside").unwrap();
    fs::write(
        fixture.root.join(format!("export.{}", LONG.to_ascii_uppercase())),
        b"outside",
    )
    .unwrap();
    assert_eq!(fixture.run(&["--category", "Long"])["matched_items"], 1);
    let mixed = fixture.run(&["--category", "Long", "--include-files"]);
    assert_eq!(mixed["matched_items"], 2);
    let names: Vec<_> = mixed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            PathBuf::from(item["path"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(names.contains(&"bundle".into()));
    assert!(names.contains(&format!("export.{}", LONG.to_ascii_uppercase()).into()));

    // The original mixed category loses its last filename rule after override.
    fs::write(
        &fixture.config,
        format!("{WITH_RULE}\n[[category]]\nname = \"New owner\"\nextensions = [\".{LONG}\"]\n"),
    )
    .unwrap();
    let mixed = fixture.run(&["--category", "Long", "--include-files"]);
    assert_eq!(mixed["matched_items"], 1);
    assert_eq!(fixture.run(&["--category", "New owner"])["matched_items"], 1);
}
