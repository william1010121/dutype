//! Minimal Model Context Protocol server over stdio (`dutype --mcp`), so LLM
//! agents can call dutype as a tool. One JSON-RPC message per line.

use crate::category::reg;
use crate::report;
use crate::scan::{self, Progress};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

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
                allocated on disk. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to scan. Absolute paths are best; ~ is expanded."},
                    "top": {"type": "integer", "minimum": 1, "maximum": 500, "default": 20, "description": "Length of each list."},
                    "cross_fs": {"type": "boolean", "default": false, "description": "Descend into other mounted filesystems."}
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
                    "cross_fs": {"type": "boolean", "default": false}
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
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

fn call_tool(name: &str, args: &Value) -> Result<Value, String> {
    let path = args.get("path").and_then(Value::as_str).ok_or("missing required argument: path")?;
    let top = args.get("top").and_then(Value::as_u64).map(|t| t.clamp(1, 500) as usize);
    let cross_fs = args.get("cross_fs").and_then(Value::as_bool).unwrap_or(false);
    let category = match name {
        "disk_usage" => None,
        "find_by_category" => {
            let c = args.get("category").and_then(Value::as_str).ok_or("missing required argument: category")?;
            Some(reg().find_by_name(c).ok_or_else(|| format!("unknown category {c:?}"))?)
        }
        _ => return Err(format!("unknown tool {name:?}")),
    };

    let path = std::fs::canonicalize(expand(path)).map_err(|e| format!("{path}: {e}"))?;
    let progress = Progress::default();
    let started = Instant::now();
    let root = scan::scan(&path, !cross_fs, &progress).map_err(|e| format!("{}: {e}", path.display()))?;
    let value = match category {
        None => {
            let errors = progress.errors.load(Ordering::Relaxed);
            serde_json::to_value(report::build(&root, &path, started.elapsed(), errors, top.unwrap_or(20)))
        }
        Some(c) => serde_json::to_value(json!({
            "path": path.display().to_string(),
            "category": c.label(),
            "items": report::find(&root, &path, c, top.unwrap_or(50)),
        })),
    };
    std::mem::forget(root);
    value.map_err(|e| e.to_string())
}

fn handle(msg: &Value) -> Option<Value> {
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
            Ok(match call_tool(name, &args) {
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

pub fn serve() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(r) = response {
            writeln!(stdout, "{r}").and_then(|_| stdout.flush()).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
