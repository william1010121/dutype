//! Minimal Model Context Protocol server over stdio (`dutype --mcp`), so LLM
//! agents can call dutype as a tool. One JSON-RPC message per line.

use crate::cache::{self, Snapshot};
use crate::category::reg;
use crate::report;
use crate::scan::{self, Progress};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools() -> Value {
    let categories: Vec<&str> = reg().all().map(|c| c.label()).collect();
    json!([
        {
            "name": "disk_usage",
            "title": "Disk usage by kind",
            "description": "Scan a directory and report how its disk usage splits by kind: \
                file types (video, PDF, archives, ML models, ...) and special directories \
                (node_modules, Python envs, .git, build output, caches, ...). Returns totals, \
                every category's share, the biggest children, the biggest special \
                directories (cleanup candidates) and the biggest files. Sizes are bytes \
                allocated on disk (size_bytes, like du). With clones: true (slower), on APFS \
                dedup_bytes counts each clone family once and freeable_min_bytes is a lower \
                bound of what deleting an item frees; without it freeable_min_bytes is null on \
                macOS. See accounting. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to scan. Absolute paths are best; ~ is expanded."},
                    "top": {"type": "integer", "minimum": 1, "maximum": 500, "default": 20, "description": "Length of each list."},
                    "cross_fs": {"type": "boolean", "default": false, "description": "Descend into other mounted filesystems."},
                    "refresh": {"type": "boolean", "default": false, "description": "Rescan even if a cached scan exists (only matters with dutype --mcp --cache; see cached_at in the result)."},
                    "clones": {"type": "boolean", "default": false, "description": "Detect APFS clones (macOS; makes the scan slower): fills dedup_bytes and freeable_min_bytes."},
                    "size_mode": {"type": "string", "enum": ["du", "dedup"], "default": "du", "description": "Rank lists by allocated size (du) or by size with APFS clones counted once (dedup; implies clones)."}
                },
                "required": ["path"]
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "find_by_category",
            "title": "Largest items of one kind",
            "description": "List the largest items of one category below a directory, e.g. every \
                node_modules or Python env with its size, or the biggest PDF / video files. \
                Directory categories return the matching directories, file categories \
                return files. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to scan."},
                    "category": {"type": "string", "enum": categories, "description": "Category name."},
                    "top": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50},
                    "cross_fs": {"type": "boolean", "default": false},
                    "refresh": {"type": "boolean", "default": false},
                    "clones": {"type": "boolean", "default": false},
                    "size_mode": {"type": "string", "enum": ["du", "dedup"], "default": "du"}
                },
                "required": ["path", "category"]
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }
    ])
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .unwrap_or_default();
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

/// Cache behaviour for the whole server, from `dutype --mcp [--cache|--refresh]`.
#[derive(Clone, Copy)]
pub struct CacheOpts {
    pub use_cache: bool,
    /// `--refresh`: every call rescans (and updates the in-memory cache).
    pub always_refresh: bool,
}

fn call_tool(name: &str, args: &Value, opts: CacheOpts) -> Result<Value, String> {
    let use_cache = opts.use_cache;
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("missing required argument: path")?;
    let top = args
        .get("top")
        .and_then(Value::as_u64)
        .map(|t| t.clamp(1, 500) as usize);
    let cross_fs = args.get("cross_fs").and_then(Value::as_bool).unwrap_or(false);
    let refresh = opts.always_refresh || args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
    let size_mode = match args.get("size_mode").filter(|v| !v.is_null()) {
        None => report::SizeMode::Du,
        Some(v) if v == "du" => report::SizeMode::Du,
        Some(v) if v == "dedup" => report::SizeMode::Dedup,
        Some(v) => return Err(format!("unknown size_mode {v} (du or dedup)")),
    };
    let clones = size_mode == report::SizeMode::Dedup || args.get("clones").and_then(Value::as_bool).unwrap_or(false);
    let category = match name {
        "disk_usage" => None,
        "find_by_category" => {
            let c = args
                .get("category")
                .and_then(Value::as_str)
                .ok_or("missing required argument: category")?;
            Some(reg().find_by_name(c).ok_or_else(|| format!("unknown category {c:?}"))?)
        }
        _ => return Err(format!("unknown tool {name:?}")),
    };

    let path = std::fs::canonicalize(expand(path)).map_err(|e| format!("{path}: {e}"))?;
    let one_fs = !cross_fs;
    let render = |snap: &Snapshot| -> Result<Value, String> {
        match category {
            None => serde_json::to_value(report::build(
                &snap.root,
                &path,
                &report::Meta {
                    elapsed: snap.scan_time,
                    cached_at: snap.cached_at,
                    unreadable: snap.errors,
                    clones: snap.clones,
                    clone_ids: snap.clone_ids,
                    size_mode,
                },
                top.unwrap_or(20),
            )),
            Some(c) => serde_json::to_value(json!({
                "path": path.display().to_string(),
                "category": c.label(),
                "cached_at": report::unix_secs(snap.cached_at),
                "items": report::find(&snap.root, &path, c, top.unwrap_or(50), size_mode, snap.clones),
            })),
        }
        .map_err(|e| e.to_string())
    };

    // The tools are read-only, so MCP never writes snapshots. With --cache it
    // reuses scans from this session (in memory) and snapshots the CLI saved.
    MEMORY.with_borrow_mut(|memory| {
        let key = (path.clone(), one_fs);
        if use_cache && !refresh {
            if let Some(snap) = memory.get(&key).filter(|s| cache::answers(s, clones)) {
                return render(snap);
            }
            if let Some(snap) = cache::load(&path, one_fs, clones) {
                let value = render(&snap);
                memory.insert(key, snap);
                return value;
            }
        }
        let progress = Progress::default();
        let started = Instant::now();
        let root_dev = scan::device_of(&path).ok();
        let root = scan::scan(&path, one_fs, clones, &progress).map_err(|e| format!("{}: {e}", path.display()))?;
        let snap = Snapshot {
            root,
            cached_at: None,
            scan_time: started.elapsed(),
            errors: progress.errors.load(Ordering::Relaxed),
            root_dev,
            clones,
            clone_ids: progress.clone_ids.load(Ordering::Relaxed),
        };
        let value = render(&snap);
        if use_cache {
            let snap = Snapshot {
                cached_at: Some(SystemTime::now()),
                ..snap
            };
            // Replacing an older tree frees it; do that off the request path.
            if let Some(old) = memory.insert(key, snap) {
                std::thread::spawn(move || drop(old));
            }
        } else {
            std::thread::spawn(move || drop(snap));
        }
        value
    })
}

thread_local! {
    /// Scans kept for the server's lifetime with `--mcp --cache`.
    static MEMORY: RefCell<HashMap<(PathBuf, bool), Snapshot>> = RefCell::new(HashMap::new());
}

fn handle(msg: &Value, opts: CacheOpts) -> Option<Value> {
    let id = msg.get("id")?.clone(); // notifications get no response
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "dutype", "title": "dutype", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Use disk_usage to see what kinds of data fill a directory, and \
                find_by_category to list e.g. every node_modules or the biggest videos. \
                Both are read-only; never delete files without the user's confirmation."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            // Tool failures are results with isError, not protocol errors.
            Ok(match call_tool(name, &args, opts) {
                Ok(v) => json!({
                    "content": [{"type": "text", "text": serde_json::to_string(&v).unwrap_or_default()}],
                    "structuredContent": v,
                    "isError": false
                }),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            })
        }
        _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

pub fn serve(opts: CacheOpts) -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, opts),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(r) = response {
            writeln!(stdout, "{r}")
                .and_then(|_| stdout.flush())
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> CacheOpts {
        CacheOpts {
            use_cache: false,
            always_refresh: false,
        }
    }

    #[test]
    fn size_mode_is_validated_and_reported() {
        let dir = std::env::temp_dir().join(format!("dutype-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), b"x").unwrap();
        let path = dir.display().to_string();
        let call = |mode: Value| call_tool("disk_usage", &json!({"path": path, "size_mode": mode}), opts());
        let r = call(json!("dedup")).unwrap();
        assert_eq!(r["accounting"]["size_mode"], "dedup");
        assert_ne!(r["accounting"]["clone_detection"], "off");
        assert_eq!(r["dedup_bytes"], r["size_bytes"]);
        assert!(r["freeable_min_bytes"].is_u64());
        // Without clones: detection off, and on macOS no freeable bound.
        let r = call(Value::Null).unwrap();
        assert_eq!(r["accounting"]["size_mode"], "du");
        assert_eq!(r["accounting"]["clone_detection"], "off");
        assert_eq!(r["freeable_min_bytes"].is_null(), cfg!(target_os = "macos"));
        assert_eq!(
            r["children"][0]["freeable_min_bytes"].is_null(),
            cfg!(target_os = "macos")
        );
        assert!(call(json!("DEDUP")).is_err());
        assert!(call(json!(1)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
