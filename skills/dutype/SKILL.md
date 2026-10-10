---
name: dutype
description: Find out what is filling a disk or directory, broken down by kind — videos, PDFs, archives, ML models, node_modules, Python envs, .git, build output, caches — using the dutype CLI. Use when the user asks why a disk is full, what takes up space in a folder, how to free space, or wants every node_modules / virtualenv / cache listed with sizes.
---

# dutype

`dutype` scans a directory in parallel and attributes every byte to a category.
Directory rules win over extensions: everything inside `node_modules/` counts as
`node_modules`, everything inside a folder with `pyvenv.cfg` counts as `Python env`.

## Commands

```bash
dutype --json PATH                        # full report as JSON
dutype --json --top 50 PATH               # longer lists
dutype --json --category node_modules PATH   # every node_modules with its size
dutype --json --category "Python env" ~      # every venv / conda env
dutype --json --category Video ~/Movies      # biggest video files
dutype --report PATH                      # same data as plain text
dutype --dump-config                      # list all categories and rules
```

Never run plain `dutype PATH` from an agent: without `--json` / `--report` it
opens an interactive TUI and waits for keyboard input.

If `dutype` is not installed, run it through npx with the same arguments:
`npx -y dutype --json PATH` (or install with `npm i -g dutype`).

## JSON report fields

- `size_bytes`, `files`, `scan_seconds`, `unreadable` (permission errors)
- `categories[]`: `{name, size_bytes, size, percent}`, biggest first
- `children[]`: direct children with `type`, `percent`, `special` (set when the
  child itself is e.g. a `node_modules`) and `top_categories`
- `special_dirs[]`: biggest node_modules / envs / caches / build output — the
  usual cleanup candidates, with absolute `path`
- `largest_files[]`: biggest individual files with their category

`--category` returns `{path, category, items: [{path, size_bytes, size}]}`.

## Workflow for "my disk is full"

1. `dutype --json --top 15 ~` (or the path the user names) and summarize the
   top categories and children in a few lines.
2. Drill into the biggest child, or use `--category` for the dominant kind.
3. Propose concrete actions with sizes: regenerable items first (`Build output`,
   `Package cache`, `Python cache`, `node_modules`, `Cache`), then user data.
4. Never delete anything without explicit confirmation of the exact paths.
   Prefer the tool's own cleanup where it exists (`cargo clean`,
   `npm cache clean --force`, `uv cache clean`, `docker system prune`).

## MCP

`dutype --mcp` is an MCP server on stdio with two read-only tools,
`disk_usage(path, top?, cross_fs?)` and `find_by_category(path, category, top?)`.
Claude Code: `claude mcp add dutype -- npx -y dutype --mcp`.
