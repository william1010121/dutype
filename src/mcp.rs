//! Minimal Model Context Protocol server over stdio (`dutype --mcp`), so LLM
//! agents can call dutype as a tool. One JSON-RPC message per line.

use crate::cache::{self, Snapshot};
use crate::category::reg;
use crate::report;
use crate::scan::{self, Progress};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, hash_map::RandomState};
use std::hash::{BuildHasher, Hash};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
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
                return files. matched_items counts matches in the scanned snapshot before \
                the top limit; a matching directory counts once, without its descendants. \
                items_truncated is true when matched_items exceeds items.length. \
                unreadable counts entries that could not be read across the \
                whole scan, not just this category: unreadable entries may be unclassifiable. \
                Optional pagination uses one retained completed scan, not an atomic filesystem snapshot. \
                Stop on pagination.has_more/next_cursor, not items_truncated. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to scan."},
                    "category": {"type": "string", "enum": categories, "description": "Category name."},
                    "top": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50},
                    "paginate": {"type": "boolean", "default": false, "description": "Opt in to snapshot-consistent sequential pages (requires --mcp --cache without --refresh)."},
                    "cursor": {"type": "string", "description": "Opaque next_cursor from the preceding page; implies pagination. Repeat its returned absolute path, category and effective options. top may change; refresh is forbidden."},
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

// JSON Schema integers include representations such as 20.0 and 2e1.
fn integral_number(value: &Value) -> bool {
    value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.is_finite() && n.fract() == 0.0)
}

fn top_value(value: &Value) -> Option<usize> {
    let number = value.as_f64()?;
    (integral_number(value) && (1.0..=500.0).contains(&number)).then_some(number as usize)
}

/// Only the current generation is retained. Cursors do not own trees or add
/// per-query/per-page state. `shares` freezes the scan's rendering context.
struct Retained {
    snapshot: Snapshot,
    generation: u64,
    shares: bool,
}

#[derive(Debug)]
enum ToolError {
    Argument(String),
    Execution(String),
    StaleCursor,
}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::Execution(message)
    }
}

impl From<&str> for ToolError {
    fn from(message: &str) -> Self {
        Self::Execution(message.into())
    }
}

/// Process-local opaque identifiers, not authentication credentials or a
/// cryptographic security boundary. Independent randomized hash seeds make a
/// new process epoch even if it reloads identical disk snapshots in the same
/// clock tick. No timestamp is used as a generation identity. As with a random
/// UUID, cross-process uniqueness is probabilistic (128 bits), not persistent.
struct CursorState {
    epoch: [u64; 2],
    query: [RandomState; 2],
    integrity: [RandomState; 2],
    next_generation: AtomicU64,
}

fn fingerprint(states: &[RandomState; 2], value: impl Hash) -> [u64; 2] {
    [states[0].hash_one(&value), states[1].hash_one(&value)]
}

impl CursorState {
    fn new() -> Self {
        Self {
            epoch: fingerprint(
                &[RandomState::new(), RandomState::new()],
                ("dutype-mcp-epoch", std::process::id()),
            ),
            query: [RandomState::new(), RandomState::new()],
            integrity: [RandomState::new(), RandomState::new()],
            next_generation: AtomicU64::new(1),
        }
    }

    fn generation(&self) -> Result<u64, ToolError> {
        // Never wrap, reuse an ID, or invalidate the previous tree on failure.
        let mut current = self.next_generation.load(Ordering::Relaxed);
        loop {
            let next = current.checked_add(1).ok_or_else(|| {
                ToolError::Execution("snapshot generation IDs exhausted; restart the MCP server".into())
            })?;
            match self
                .next_generation
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(current),
                Err(actual) => current = actual,
            }
        }
    }

    fn snapshot_id(&self, generation: u64) -> String {
        format!("{:016x}{:016x}-{:016x}", self.epoch[0], self.epoch[1], generation)
    }
}

fn cursor_state() -> &'static CursorState {
    static STATE: OnceLock<CursorState> = OnceLock::new();
    STATE.get_or_init(CursorState::new)
}

#[derive(Clone, Copy, Hash)]
struct Cursor {
    epoch: [u64; 2],
    generation: u64,
    query: [u64; 2],
    after: u64,
}

impl Cursor {
    // Fixed-width lower-case hex rejects alternate encodings, unbounded input,
    // unknown versions, numeric overflow, zero ordinals, and trailing fields.
    fn parse(token: &str) -> Result<(Self, [u64; 2]), ToolError> {
        const LEN: usize = 3 + 8 * 16;
        let invalid = || ToolError::Argument("invalid or unsupported category cursor".into());
        if token.len() != LEN || !token.starts_with("v1.") {
            return Err(invalid());
        }
        let body = &token.as_bytes()[3..];
        if !body.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)) {
            return Err(invalid());
        }
        let mut fields = [0; 8];
        for (field, chunk) in fields.iter_mut().zip(body.as_chunks::<16>().0) {
            *field =
                u64::from_str_radix(std::str::from_utf8(chunk).map_err(|_| invalid())?, 16).map_err(|_| invalid())?;
        }
        if fields[2] == 0 || fields[5] == 0 {
            return Err(invalid());
        }
        Ok((
            Self {
                epoch: [fields[0], fields[1]],
                generation: fields[2],
                query: [fields[3], fields[4]],
                after: fields[5],
            },
            [fields[6], fields[7]],
        ))
    }

    fn encode(self, state: &CursorState) -> String {
        let tag = fingerprint(&state.integrity, self);
        format!(
            "v1.{:016x}{:016x}{:016x}{:016x}{:016x}{:016x}{:016x}{:016x}",
            self.epoch[0], self.epoch[1], self.generation, self.query[0], self.query[1], self.after, tag[0], tag[1]
        )
    }

    fn decode(token: &str, state: &CursorState) -> Result<Self, ToolError> {
        let (cursor, tag) = Self::parse(token)?;
        if cursor.epoch != state.epoch {
            return Err(ToolError::StaleCursor);
        }
        if fingerprint(&state.integrity, cursor) != tag {
            return Err(ToolError::Argument("invalid category cursor fingerprint".into()));
        }
        Ok(cursor)
    }
}

/// Keep all filesystem entry points out of continuation handling. The test
/// counter covers canonicalization, sharing probes, cache loading, device
/// lookup and scanning (including clone/private I/O inside that scan).
fn filesystem<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    FILESYSTEM_CALLS.with(|count| count.set(count.get() + 1));
    f()
}

fn call_tool(name: &str, args: &Value, opts: CacheOpts) -> Result<Value, ToolError> {
    let use_cache = opts.use_cache;
    let requested_path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("missing required argument: path")?;
    let top = args
        .get("top")
        .map(|value| top_value(value).ok_or("top must be an integer from 1 to 500"))
        .transpose()?;
    let cross_fs = args.get("cross_fs").and_then(Value::as_bool).unwrap_or(false);
    let refresh = opts.always_refresh || args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
    let private = args.get("private").and_then(Value::as_bool).unwrap_or(false);
    let size_mode = match args.get("size_mode").filter(|v| !v.is_null()) {
        None => report::SizeMode::Du,
        Some(v) if v == "du" => report::SizeMode::Du,
        Some(v) if v == "dedup" => report::SizeMode::Dedup,
        Some(v) => return Err(format!("unknown size_mode {v} (du or dedup)").into()),
    };
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
        _ => return Err(format!("unknown tool {name:?}").into()),
    };
    let token = args.get("cursor").and_then(Value::as_str);
    let paginate = category.is_some() && (token.is_some() || args.get("paginate") == Some(&Value::Bool(true)));
    if paginate && (!use_cache || opts.always_refresh) {
        return Err("category pagination requires dutype --mcp --cache without --refresh".into());
    }
    let state = cursor_state();
    let cursor = token.map(|token| Cursor::decode(token, state)).transpose()?;
    let one_fs = !cross_fs;
    let query = |path: &std::path::Path, c: crate::category::Category| {
        fingerprint(
            &state.query,
            (path, c, size_mode == report::SizeMode::Dedup, one_fs, clones, private),
        )
    };
    let render =
        |snap: &Snapshot, path: &std::path::Path, shares: bool, generation: Option<u64>| -> Result<Value, ToolError> {
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
            let Some(c) = category else {
                return serde_json::to_value(report::build(&snap.root, path, &meta, top.unwrap_or(20)))
                    .map_err(|e| e.to_string().into());
            };
            let known = report::freeable_known(report::detected(snap.clones, snap.clone_ids, snap.reflink_fs), shares);
            let (matched_items, items_truncated, items, pagination) = if paginate {
                let generation = generation.ok_or("pagination needs a retained snapshot")?;
                let page = report::find_page(
                    &snap.root,
                    path,
                    c,
                    top.unwrap_or(50),
                    size_mode,
                    known,
                    meta.private,
                    cursor.map(|cursor| cursor.after),
                )
                .map_err(|e| ToolError::Argument(format!("invalid category cursor boundary: {e}")))?;
                let next_cursor = page.next_after.map(|after| {
                    Cursor {
                        epoch: state.epoch,
                        generation,
                        query: query(path, c),
                        after,
                    }
                    .encode(state)
                });
                (
                    page.matched_items,
                    page.items_truncated,
                    page.items,
                    Some(json!({
                        "snapshot_id": state.snapshot_id(generation),
                        "offset": page.offset,
                        "has_more": page.has_more,
                        "next_cursor": next_cursor,
                    })),
                )
            } else {
                let found = report::find(&snap.root, path, c, top.unwrap_or(50), size_mode, known, meta.private);
                (found.matched_items, found.items_truncated, found.items, None)
            };
            let mut result = json!({
                "path": path.display().to_string(),
                "category": c.label(),
                "cached_at": report::unix_secs(snap.cached_at),
                "unreadable": meta.unreadable,
                "accounting": report::accounting(&meta),
                "matched_items": matched_items,
                "items_truncated": items_truncated,
                "items": items,
            });
            if let Some(pagination) = pagination {
                result["pagination"] = pagination;
            }
            Ok(result)
        };

    if let Some(cursor) = cursor {
        // No expansion, canonicalization, filesystem probe, cache read or scan.
        // Find only a current generation; old trees are never pinned by cursors.
        return MEMORY.with_borrow(|memory| {
            let ((path, stored_one_fs), retained) = memory
                .iter()
                .find(|(_, retained)| retained.generation == cursor.generation)
                .ok_or(ToolError::StaleCursor)?;
            if requested_path != path.display().to_string()
                || one_fs != *stored_one_fs
                || cursor.query != query(path, category.ok_or("cursor needs a category")?)
            {
                return Err(ToolError::Argument(
                    "cursor query does not match; repeat the returned absolute path, category and effective options"
                        .into(),
                ));
            }
            render(&retained.snapshot, path, retained.shares, Some(retained.generation))
        });
    }

    let path =
        filesystem(|| std::fs::canonicalize(expand(requested_path))).map_err(|e| format!("{requested_path}: {e}"))?;
    let shares = filesystem(|| scan::may_share_blocks(&path, one_fs));
    // MCP never writes disk snapshots. --cache retains one current tree per
    // canonical root/filesystem policy, with no per-cursor state.
    MEMORY.with_borrow_mut(|memory| {
        let key = (path.clone(), one_fs);
        if use_cache && !refresh {
            let fits = |s: &Snapshot| cache::answers(s, clones) && s.serves(private);
            if let Some(retained) = memory.get(&key).filter(|r| fits(&r.snapshot)) {
                return render(
                    &retained.snapshot,
                    &path,
                    if paginate { retained.shares } else { shares },
                    Some(retained.generation),
                );
            }
            if let Some(snap) = filesystem(|| cache::load(&path, one_fs, clones)).filter(|s| s.serves(private)) {
                let retained = Retained {
                    snapshot: snap,
                    generation: state.generation()?,
                    shares,
                };
                let value = render(&retained.snapshot, &path, shares, Some(retained.generation));
                if let Some(old) = memory.insert(key, retained) {
                    std::thread::spawn(move || drop(old));
                }
                return value;
            }
        }
        let progress = Progress {
            private,
            ..Progress::default()
        };
        let started = Instant::now();
        let root_dev = filesystem(|| scan::device_of(&path)).ok();
        let root = filesystem(|| scan::scan(&path, one_fs, clones, &progress))
            .map_err(|e| format!("{}: {e}", path.display()))?;
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
        let generation = use_cache.then(|| state.generation()).transpose()?;
        let value = render(&snap, &path, shares, generation);
        if let Some(generation) = generation {
            let snap = Snapshot {
                cached_at: Some(SystemTime::now()),
                ..snap
            };
            if let Some(old) = memory.insert(
                key,
                Retained {
                    snapshot: snap,
                    generation,
                    shares,
                },
            ) {
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
    static MEMORY: RefCell<HashMap<(PathBuf, bool), Retained>> = RefCell::new(HashMap::new());
    #[cfg(test)]
    static FILESYSTEM_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
    if name == "find_by_category" {
        if args.get("paginate").is_some_and(|value| !value.is_boolean()) {
            return Err("paginate must be a boolean".into());
        }
        if let Some(cursor) = args.get("cursor") {
            let cursor = cursor.as_str().ok_or("cursor must be a string")?;
            Cursor::parse(cursor).map_err(|_| "invalid or unsupported category cursor")?;
            if args.get("paginate") == Some(&Value::Bool(false)) {
                return Err("cursor cannot be combined with paginate: false".into());
            }
            if args.get("refresh") == Some(&Value::Bool(true)) {
                return Err("cursor cannot be combined with refresh: true".into());
            }
        }
    } else if args.get("paginate").is_some() || args.get("cursor").is_some() {
        return Err("pagination is only supported by find_by_category".into());
    }
    if let Some(top) = args.get("top")
        && top_value(top).is_none()
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
        Err(ToolError::Argument(error)) => return Err(rpc_error(-32602, error)),
        Err(ToolError::Execution(error)) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
        Err(ToolError::StaleCursor) => {
            let error = json!({"code": "stale_cursor", "restart_required": true,
                "message": "The cursor snapshot is no longer retained; start again without a cursor."});
            json!({"content": [{"type": "text", "text": error.to_string()}], "structuredContent": error, "isError": true})
        }
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
        || id.is_some_and(|id| !(id.is_string() || integral_number(id)))
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

    struct PaginationFixture {
        path: PathBuf,
    }

    impl PaginationFixture {
        fn new(shares: bool, private: bool) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "dutype-pagination-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            for name in ["a.pdf", "b.pdf", "c.pdf"] {
                std::fs::write(dir.join(name), b"pdf").unwrap();
            }
            let path = std::fs::canonicalize(dir).unwrap();
            let root = scan::scan_excluding(&path, true, false, &Progress::default(), None).unwrap();
            let mut privates = scan::PrivateSizes::default();
            for (i, name) in ["a.pdf", "b.pdf", "c.pdf"].into_iter().enumerate() {
                privates.0.insert(PathBuf::from(name), i as u64 + 10);
            }
            let snapshot = Snapshot {
                root,
                cached_at: Some(SystemTime::UNIX_EPOCH),
                scan_time: std::time::Duration::ZERO,
                errors: 7,
                root_dev: None,
                clones: private,
                clone_ids: private,
                clone_gaps: private,
                private,
                private_sizes: private,
                private_missing: false,
                privates,
                reflink_fs: None,
            };
            MEMORY.with_borrow_mut(|memory| {
                memory.insert(
                    (path.clone(), true),
                    Retained {
                        snapshot,
                        generation: cursor_state().generation().unwrap(),
                        shares,
                    },
                );
            });
            Self { path }
        }

        fn args(&self) -> Value {
            json!({"path": self.path, "category": "PDF", "paginate": true, "top": 1})
        }

        fn call(&self, args: Value) -> Result<Value, Value> {
            handle_tool_call(
                &json!({"name": "find_by_category", "arguments": args}),
                CacheOpts {
                    use_cache: true,
                    always_refresh: false,
                },
            )
        }

        fn page(&self, args: Value) -> Value {
            let result = self.call(args).unwrap();
            assert_eq!(result["isError"], false, "{result}");
            let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(text, result["structuredContent"]);
            result["structuredContent"].clone()
        }
    }

    impl Drop for PaginationFixture {
        fn drop(&mut self) {
            MEMORY.with_borrow_mut(|memory| {
                memory.remove(&(self.path.clone(), true));
            });
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn paginated_metadata_and_frozen_context_survive_without_filesystem_work() {
        for (shares, private) in [(false, false), (true, false), (true, true)] {
            let fixture = PaginationFixture::new(shares, private);
            let mut args = fixture.args();
            args["private"] = json!(private);
            let first = fixture.page(args.clone());
            assert_eq!(first["unreadable"], 7);
            assert_eq!(first["accounting"]["private"], private);
            assert_eq!(first["items"][0]["private_bytes"].is_number(), private);
            assert_eq!(first["items"][0]["freeable_min_bytes"].is_null(), shares && !private);
            assert_eq!(first["pagination"]["offset"], 0);
            std::fs::remove_dir_all(&fixture.path).unwrap();
            let before = MEMORY.with_borrow(HashMap::len);
            let mut page = first.clone();
            for offset in [1, 2] {
                args["cursor"] = page["pagination"]["next_cursor"].clone();
                FILESYSTEM_CALLS.with(|count| count.set(0));
                page = fixture.page(args.clone());
                assert_eq!(FILESYSTEM_CALLS.with(std::cell::Cell::get), 0);
                assert_eq!(page["unreadable"], first["unreadable"]);
                assert_eq!(page["accounting"], first["accounting"]);
                assert_eq!(page["cached_at"], first["cached_at"]);
                assert_eq!(page["matched_items"], 3);
                assert_eq!(page["items_truncated"], true);
                assert_eq!(page["pagination"]["snapshot_id"], first["pagination"]["snapshot_id"]);
                assert_eq!(page["pagination"]["offset"], offset);
                assert_eq!(page["items"][0]["freeable_min_bytes"].is_null(), shares && !private);
                assert_eq!(page["items"][0]["private_bytes"].is_number(), private);
            }
            assert_eq!(page["pagination"]["has_more"], false);
            assert!(page["pagination"]["next_cursor"].is_null());
            assert_eq!(MEMORY.with_borrow(HashMap::len), before, "pages add no retained state");
        }
    }

    #[test]
    fn invalid_page_queries_and_tokens_never_touch_the_filesystem() {
        let fixture = PaginationFixture::new(false, false);
        let first = fixture.page(fixture.args());
        let token = first["pagination"]["next_cursor"].as_str().unwrap();
        let mut base = fixture.args();
        base["cursor"] = json!(token);
        for (key, value) in [
            ("path", json!(fixture.path.join("missing"))),
            ("path", json!(format!("{}/.", fixture.path.display()))),
            ("category", json!("Video")),
            ("size_mode", json!("dedup")),
            ("clones", json!(true)),
            ("private", json!(true)),
            ("cross_fs", json!(true)),
            ("paginate", json!(false)),
            ("refresh", json!(true)),
            ("cursor", Value::Null),
            ("cursor", json!(false)),
            ("cursor", json!("")),
            ("cursor", json!(format!("{token}0"))),
            ("cursor", json!(token.to_uppercase())),
            ("cursor", json!(token.replacen("v1.", "v2.", 1))),
            ("cursor", json!("x".repeat(10_000))),
        ] {
            let mut args = base.clone();
            args[key] = value;
            FILESYSTEM_CALLS.with(|count| count.set(0));
            let error = fixture.call(args.clone()).unwrap_err();
            assert_eq!(error["code"], -32602, "{args}: {error}");
            assert_eq!(FILESYSTEM_CALLS.with(std::cell::Cell::get), 0);
        }
        let (mut cursor, _) = Cursor::parse(token).unwrap();
        // A canonical but tampered ordinal must fail the fingerprint before
        // even resolving a tree boundary. Internally generated out-of-range
        // boundaries are independently rejected by the bounded report helper.
        let mut tampered = token.as_bytes().to_vec();
        tampered[3 + 5 * 16] = if tampered[3 + 5 * 16] == b'0' { b'1' } else { b'0' };
        for bad in [String::from_utf8(tampered).unwrap(), {
            cursor.after = u64::MAX;
            cursor.encode(cursor_state())
        }] {
            let mut args = base.clone();
            args["cursor"] = json!(bad);
            FILESYSTEM_CALLS.with(|count| count.set(0));
            assert_eq!(fixture.call(args).unwrap_err()["code"], -32602);
            assert_eq!(FILESYSTEM_CALLS.with(std::cell::Cell::get), 0);
        }
        for invalid in [json!(1), json!("true"), Value::Null] {
            let mut args = fixture.args();
            args["paginate"] = invalid;
            FILESYSTEM_CALLS.with(|count| count.set(0));
            assert_eq!(fixture.call(args).unwrap_err()["code"], -32602);
            assert_eq!(FILESYSTEM_CALLS.with(std::cell::Cell::get), 0);
        }
    }

    #[test]
    fn stale_cursors_are_machine_readable_execution_errors_without_scans() {
        let fixture = PaginationFixture::new(false, false);
        let first = fixture.page(fixture.args());
        let (cursor, _) = Cursor::parse(first["pagination"]["next_cursor"].as_str().unwrap()).unwrap();
        for stale in [
            Cursor {
                generation: u64::MAX,
                ..cursor
            },
            Cursor {
                epoch: [!cursor.epoch[0], cursor.epoch[1]],
                ..cursor
            },
        ] {
            let mut args = fixture.args();
            args["cursor"] = json!(stale.encode(cursor_state()));
            FILESYSTEM_CALLS.with(|count| count.set(0));
            let result = fixture.call(args).unwrap();
            assert_eq!(result["isError"], true);
            assert_eq!(result["structuredContent"]["code"], "stale_cursor");
            assert_eq!(result["structuredContent"]["restart_required"], true);
            let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(text, result["structuredContent"]);
            assert_eq!(FILESYSTEM_CALLS.with(std::cell::Cell::get), 0);
        }
    }

    #[test]
    fn process_epoch_and_generation_do_not_depend_on_cached_timestamp_or_wrap() {
        let a = CursorState::new();
        let b = CursorState::new();
        assert_ne!(a.epoch, b.epoch);
        assert_ne!(a.snapshot_id(1), b.snapshot_id(1));
        assert_ne!(a.snapshot_id(1), a.snapshot_id(2));
        a.next_generation.store(u64::MAX - 1, Ordering::Relaxed);
        assert_eq!(a.generation().unwrap(), u64::MAX - 1);
        assert!(matches!(a.generation(), Err(ToolError::Execution(_))));
        assert!(matches!(a.generation(), Err(ToolError::Execution(_))));
        assert_eq!(a.next_generation.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn pagination_requires_cache_and_is_not_a_disk_usage_option() {
        let args = json!({"path": "nonexistent-pagination-root", "category": "PDF", "paginate": true});
        for opts in [
            CacheOpts {
                use_cache: false,
                always_refresh: false,
            },
            CacheOpts {
                use_cache: true,
                always_refresh: true,
            },
        ] {
            FILESYSTEM_CALLS.with(|count| count.set(0));
            let result = handle_tool_call(&json!({"name": "find_by_category", "arguments": args}), opts).unwrap();
            assert_eq!(result["isError"], true);
            assert!(
                result["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("--mcp --cache without --refresh")
            );
            assert_eq!(FILESYSTEM_CALLS.with(std::cell::Cell::get), 0);
        }
        assert!(validate_tool_args("disk_usage", &args).is_err());
        let schemas = tools();
        assert!(schemas[0]["inputSchema"]["properties"].get("paginate").is_none());
        assert_eq!(schemas[1]["inputSchema"]["properties"]["paginate"]["default"], false);
        assert_eq!(schemas[1]["inputSchema"]["properties"]["cursor"]["type"], "string");
    }

    #[test]
    fn category_results_serialize_scan_wide_snapshot_errors_including_empty_matches() {
        use std::time::{Duration, UNIX_EPOCH};

        let dir = std::env::temp_dir().join(format!("dutype-mcp-scan-errors-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("paper.pdf"), b"pdf").unwrap();
        let path = std::fs::canonicalize(&dir).unwrap();
        let key = (path.clone(), true);
        let cached = UNIX_EPOCH + Duration::from_secs(1_234_567_890);
        let mut expected_items = None;
        for cached_at in [None, Some(cached)] {
            for errors in [0, 7, u64::MAX] {
                let root = scan::scan_excluding(&path, true, false, &Progress::default(), None).unwrap();
                assert_eq!(root.errors(), 0);
                // Synthetic scan-wide metadata makes this test portable even
                // where chmod(000) cannot make a directory unreadable. The
                // response must use Snapshot.errors, not category matches or
                // the root's independently stored subtree error count.
                let snap = Snapshot {
                    root,
                    cached_at,
                    scan_time: Duration::ZERO,
                    errors,
                    root_dev: None,
                    clones: false,
                    clone_ids: false,
                    clone_gaps: false,
                    private: false,
                    private_sizes: false,
                    private_missing: false,
                    privates: scan::PrivateSizes::default(),
                    reflink_fs: None,
                };
                MEMORY.with_borrow_mut(|memory| {
                    memory.insert(
                        key.clone(),
                        Retained {
                            snapshot: snap,
                            generation: cursor_state().generation().unwrap(),
                            shares: scan::may_share_blocks(&path, true),
                        },
                    )
                });
                for (tool, category) in [
                    ("disk_usage", "PDF"),
                    ("find_by_category", "PDF"),
                    ("find_by_category", "Video"),
                ] {
                    let response = handle(
                        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                            "params": {"name": tool, "arguments": {"path": path, "category": category}}}),
                        CacheOpts {
                            use_cache: true,
                            always_refresh: false,
                        },
                    )
                    .unwrap();
                    let result = &response["result"];
                    assert_eq!(result["isError"], false, "{response}");
                    let content = &result["structuredContent"];
                    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
                    assert_eq!(&text, content);
                    assert_eq!(content["unreadable"].as_u64(), Some(errors));
                    assert_eq!(content["cached_at"], json!(report::unix_secs(cached_at)));
                    if tool == "find_by_category" {
                        assert!(content.get("complete").is_none());
                        if category == "Video" {
                            assert_eq!(content["items"], json!([]));
                        } else if let Some(expected) = &expected_items {
                            assert_eq!(&content["items"], expected);
                        } else {
                            assert_eq!(content["items"].as_array().unwrap().len(), 1);
                            expected_items = Some(content["items"].clone());
                        }
                    }
                }
            }
        }
        MEMORY.with_borrow_mut(|memory| memory.remove(&key));
        std::fs::remove_dir_all(dir).unwrap();
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
        for id in [
            json!(0),
            json!(-1),
            json!(1.0),
            json!(-2.0),
            json!("request\n\"id"),
            json!(u64::MAX),
        ] {
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
    fn integral_numeric_top_values_are_valid_and_honored() {
        let dir = std::env::temp_dir().join(format!("dutype-mcp-integral-top-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        for i in 0..25 {
            std::fs::write(dir.join(format!("file-{i}.pdf")), b"pdf").unwrap();
        }
        let mut results = Vec::new();
        for (value, expected) in [
            (json!(1.0), 1),
            (json!(20.0), 20),
            (json!(500.0), 25),
            (serde_json::from_str("1e0").unwrap(), 1),
        ] {
            for tool in ["disk_usage", "find_by_category"] {
                let response = handle(
                    &json!({"jsonrpc":"2.0", "id":"integral", "method":"tools/call",
                    "params":{"name":tool, "arguments":{"path":dir, "category":"PDF", "top":value}}}),
                    opts(),
                )
                .unwrap();
                results.push((tool, expected, response));
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
        for (tool, expected, response) in results {
            assert_eq!(response["result"]["isError"], false, "{response}");
            let key = if tool == "disk_usage" { "largest_files" } else { "items" };
            assert_eq!(
                response["result"]["structuredContent"][key].as_array().unwrap().len(),
                expected
            );
        }
        for value in [json!(0.0), json!(501.0), json!(-1.0), json!(1.5), json!(1e100)] {
            assert!(
                validate_tool_args("disk_usage", &json!({"path":".", "top":value})).is_err(),
                "{value}"
            );
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
