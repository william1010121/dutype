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
                allocated on disk (size_bytes, like du). With clones: true (slower), \
                dedup_bytes counts each APFS clone family (macOS) or each shared extent \
                (Linux btrfs/XFS/bcachefs reflinks and snapshots) once and \
                freeable_min_bytes is a lower bound of what deleting an item frees; without \
                it freeable_min_bytes is null on macOS and on those Linux filesystems, and \
                leaves out hard-linked files. With private: true (macOS/APFS; implies \
                clones), files that may share blocks but are not full clones get \
                private_bytes: what deleting that file alone frees right now. See \
                accounting. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to scan. Absolute paths are best; ~ is expanded."},
                    "top": {"type": "integer", "minimum": 1, "maximum": 500, "default": 20, "description": "Length of each list."},
                    "cross_fs": {"type": "boolean", "default": false, "description": "Descend into other mounted filesystems."},
                    "refresh": {"type": "boolean", "default": false, "description": "Rescan even if a cached scan exists (only matters with dutype --mcp --cache; see cached_at in the result)."},
                    "clones": {"type": "boolean", "default": false, "description": "Detect shared blocks (makes the scan slower): APFS clones on macOS, shared extents (reflinks, snapshots; one FIEMAP ioctl per file) on Linux btrfs/XFS/bcachefs. Fills dedup_bytes and freeable_min_bytes."},
                    "size_mode": {"type": "string", "enum": ["du", "dedup"], "default": "du", "description": "Rank lists by allocated size (du) or by size with APFS clones counted once (dedup; implies clones)."},
                    "private": {"type": "boolean", "default": false, "description": "Also read the APFS private size (private_bytes) of partial clones: bytes freed immediately if that file alone is deleted; affected by APFS snapshots, never summed into folders. Slower; implies clones; macOS only (elsewhere accepted, no values; see accounting.private)."}
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
                    "size_mode": {"type": "string", "enum": ["du", "dedup"], "default": "du"},
                    "private": {"type": "boolean", "default": false, "description": "Add private_bytes to file items (see disk_usage)."}
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
    let private = args.get("private").and_then(Value::as_bool).unwrap_or(false);
    let size_mode = match args.get("size_mode").filter(|v| !v.is_null()) {
        None => report::SizeMode::Du,
        Some(v) if v == "du" => report::SizeMode::Du,
        Some(v) if v == "dedup" => report::SizeMode::Dedup,
        Some(v) => return Err(format!("unknown size_mode {v} (du or dedup)")),
    };
    // Private sizes need the clone flags to pick partial clones.
    let clones =
        size_mode == report::SizeMode::Dedup || private || args.get("clones").and_then(Value::as_bool).unwrap_or(false);
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
    let shares = scan::may_share_blocks(&path, one_fs);
    let render = |snap: &Snapshot| -> Result<Value, String> {
        let meta = report::Meta {
            elapsed: snap.scan_time,
            cached_at: snap.cached_at,
            unreadable: snap.errors,
            clones: snap.clones,
            clone_ids: snap.clone_ids,
            clone_gaps: snap.clone_gaps,
            shares,
            size_mode,
            private: snap.private_table(),
            reflink_fs: snap.reflink_fs,
        };
        match category {
            None => serde_json::to_value(report::build(&snap.root, &path, &meta, top.unwrap_or(20))),
            Some(c) => serde_json::to_value(json!({
                "path": path.display().to_string(),
                "category": c.label(),
                "cached_at": report::unix_secs(snap.cached_at),
                "accounting": report::accounting(&meta),
                "items": report::find(
                    &snap.root,
                    &path,
                    c,
                    top.unwrap_or(50),
                    size_mode,
                    report::freeable_known(report::detected(snap.clones, snap.clone_ids, snap.reflink_fs), shares),
                    meta.private,
                ),
            })),
        }
        .map_err(|e| e.to_string())
    };

    // The tools are read-only, so MCP never writes snapshots. With --cache it
    // reuses scans from this session (in memory) and snapshots the CLI saved.
    MEMORY.with_borrow_mut(|memory| {
        let key = (path.clone(), one_fs);
        if use_cache && !refresh {
            let fits = |s: &Snapshot| cache::answers(s, clones) && s.serves(private);
            if let Some(snap) = memory.get(&key).filter(|s| fits(s)) {
                return render(snap);
            }
            if let Some(snap) = cache::load(&path, one_fs, clones).filter(|s| s.serves(private)) {
                let value = render(&snap);
                memory.insert(key, snap);
                return value;
            }
        }
        let progress = Progress {
            private,
            ..Progress::default()
        };
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
            clone_gaps: progress.clone_gaps.load(Ordering::Relaxed),
            private,
            private_sizes: progress.private_sizes.load(Ordering::Relaxed),
            private_missing: progress.private_missing.load(Ordering::Relaxed),
            privates: progress.take_private(),
            reflink_fs: scan::ReflinkFs::from_code(progress.reflink_fs.load(Ordering::Relaxed)),
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

fn rpc_error(code: i32, message: impl Into<String>) -> Value {
    json!({"code": code, "message": message.into()})
}

/// Reject malformed calls before touching the filesystem. Protocol errors are
/// distinct from a valid tool call that fails to scan its requested path.
fn validate_tool_args(name: &str, args: &Value) -> Result<(), String> {
    if !matches!(name, "disk_usage" | "find_by_category") {
        return Err(format!("unknown tool {name:?}"));
    }
    if !args.is_object() {
        return Err("arguments must be an object".into());
    }
    if args.get("path").and_then(Value::as_str).is_none() {
        return Err("missing or invalid required argument: path".into());
    }
    if name == "find_by_category" {
        let category = args
            .get("category")
            .and_then(Value::as_str)
            .ok_or("missing or invalid required argument: category")?;
        if reg().find_by_name(category).is_none() {
            return Err(format!("unknown category {category:?}"));
        }
    }
    if let Some(top) = args.get("top")
        && !top.as_u64().is_some_and(|top| (1..=500).contains(&top))
    {
        return Err("top must be an integer from 1 to 500".into());
    }
    for name in ["cross_fs", "refresh", "clones", "private"] {
        if args.get(name).is_some_and(|value| !value.is_boolean()) {
            return Err(format!("{name} must be a boolean"));
        }
    }
    if let Some(mode) = args.get("size_mode")
        && !matches!(mode.as_str(), Some("du" | "dedup"))
    {
        return Err("size_mode must be du or dedup".into());
    }
    Ok(())
}

fn handle_tool_call(params: &Value, opts: CacheOpts) -> Result<Value, Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| rpc_error(-32602, "missing or invalid tool name"))?;
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    validate_tool_args(name, args).map_err(|e| rpc_error(-32602, e))?;
    // Only execution failures are results with isError, not protocol errors.
    Ok(match call_tool(name, args, opts) {
        Ok(value) => json!({
            "content": [{"type": "text", "text": value.to_string()}],
            "structuredContent": value,
            "isError": false
        }),
        Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
    })
}

fn handle(msg: &Value, opts: CacheOpts) -> Option<Value> {
    let id = msg.get("id");
    // MCP requires string or integer request IDs (unlike JSON-RPC, not null).
    // Validate before deciding that a missing ID denotes a notification: e.g.
    // `null`, `{}` and a numeric method must receive Invalid Request errors.
    if !msg.is_object()
        || msg.get("jsonrpc") != Some(&json!("2.0"))
        || msg.get("method").and_then(Value::as_str).is_none()
        || id.is_some_and(|id| !(id.is_string() || id.is_i64() || id.is_u64()))
    {
        return Some(json!({"jsonrpc": "2.0", "id": null, "error": rpc_error(-32600, "invalid request")}));
    }
    let id = id?.clone(); // valid notifications get no response
    if msg.get("params").is_some_and(|params| !params.is_object()) {
        return Some(json!({"jsonrpc": "2.0", "id": id, "error": rpc_error(-32602, "params must be an object")}));
    }
    let method = msg["method"].as_str().unwrap();
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
        "tools/call" => handle_tool_call(&msg["params"], opts),
        _ => Err(rpc_error(-32601, format!("method not found: {method}"))),
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
    fn invalid_requests_get_protocol_errors_instead_of_disappearing_or_succeeding() {
        for message in [
            Value::Null,
            json!(42),
            json!([]),
            json!({}),
            json!({"jsonrpc": "2.0", "method": 7}),
            json!({"id": 1, "method": "ping"}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": true, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": [], "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"}),
        ] {
            let response = handle(&message, opts()).expect("invalid requests must not be lost as notifications");
            assert_eq!(response["error"]["code"], -32600, "{message}: {response}");
            assert_eq!(response["id"], Value::Null, "{message}: {response}");
            assert!(response.get("result").is_none(), "{response}");
        }
    }

    #[test]
    fn notifications_are_silent_and_request_ids_are_preserved() {
        for method in ["notifications/initialized", "ping", "unknown"] {
            assert!(handle(&json!({"jsonrpc": "2.0", "method": method}), opts()).is_none());
        }
        for id in [json!(0), json!(-1), json!("request\n\"id"), json!(u64::MAX)] {
            let response = handle(&json!({"jsonrpc": "2.0", "id": id, "method": "ping"}), opts()).unwrap();
            assert_eq!(response, json!({"jsonrpc": "2.0", "id": id, "result": {}}));
        }
    }

    #[test]
    fn invalid_tool_call_params_get_protocol_errors() {
        for params in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"name": 42}),
            json!({"name": "not_a_tool"}),
            json!({"name": "disk_usage", "arguments": []}),
            json!({"name": "disk_usage", "arguments": {}}),
            json!({"name": "disk_usage", "arguments": {"path": false}}),
            json!({"name": "find_by_category", "arguments": {"path": "."}}),
            json!({"name": "find_by_category", "arguments": {"path": ".", "category": "not_a_category"}}),
        ] {
            let response = handle(
                &json!({"jsonrpc": "2.0", "id": "bad-call", "method": "tools/call", "params": params}),
                opts(),
            )
            .unwrap();
            assert_eq!(response["error"]["code"], -32602, "{params}: {response}");
            assert_eq!(response["id"], "bad-call");
            assert!(response.get("result").is_none(), "{response}");
        }
    }

    #[test]
    fn tool_options_must_match_the_advertised_schema() {
        let path = std::env::temp_dir().join(format!("dutype-mcp-invalid-options-{}", std::process::id()));
        for (name, values) in [
            (
                "top",
                vec![json!(0), json!(501), json!(-1), json!(1.5), json!("5"), Value::Null],
            ),
            ("cross_fs", vec![json!("true"), json!(1), Value::Null]),
            ("refresh", vec![json!("true"), json!(1), Value::Null]),
            ("clones", vec![json!("true"), json!(1), Value::Null]),
            ("private", vec![json!("true"), json!(1), Value::Null]),
            ("size_mode", vec![json!("DEDUP"), json!(1), Value::Null]),
        ] {
            for value in values {
                let mut args = json!({"path": path});
                args[name] = value;
                let response = handle(
                    &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "disk_usage", "arguments": args}}),
                    opts(),
                )
                .unwrap();
                assert_eq!(response["error"]["code"], -32602, "{args}: {response}");
            }
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
        // Known where APFS reported clone flags, or where nothing shares blocks.
        let shares = scan::may_share_blocks(&dir, true);
        let known = r["accounting"]["clone_detection"] == "apfs-full" || !shares;
        assert_eq!(r["freeable_min_bytes"].is_u64(), known);
        if cfg!(target_os = "macos") {
            assert!(known, "temp dir on APFS expected");
        }
        // Without clones: detection off, and no freeable bound where files
        // may share blocks (always on macOS).
        let r = call(Value::Null).unwrap();
        assert_eq!(r["accounting"]["size_mode"], "du");
        assert_eq!(r["accounting"]["clone_detection"], "off");
        assert_eq!(r["freeable_min_bytes"].is_null(), shares);
        assert_eq!(r["children"][0]["freeable_min_bytes"].is_null(), shares);
        assert!(shares || !cfg!(target_os = "macos"));
        assert!(call(json!("DEDUP")).is_err());
        assert!(call(json!(1)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
