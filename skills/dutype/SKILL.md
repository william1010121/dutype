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
dutype --json --clones PATH               # macOS: also detect APFS clones (slower)
dutype --json --size-mode dedup PATH      # rank by size with APFS clones counted once (implies --clones)
dutype --report PATH                      # same data as plain text
dutype --dump-config                      # list all categories and rules
```

Never run plain `dutype PATH` from an agent: without `--json` / `--report` it
opens an interactive TUI and waits for keyboard input.

If `dutype` is not installed, run it through npx with the same arguments:
`npx -y dutype --json PATH` (or install with `npm i -g dutype`).

## JSON report fields

- `size_bytes`, `files`, `scan_seconds`, `unreadable` (permission errors)
- `dedup_bytes`, `clone_shared_bytes`, `freeable_min_bytes`, `accounting`
  (see "Shared blocks" below)
- `categories[]`: `{name, size_bytes, size, percent}`, biggest first
- `children[]`: direct children with `type`, `percent`, `special` (set when the
  child itself is e.g. a `node_modules`) and `top_categories`
- `special_dirs[]`: biggest node_modules / envs / caches / build output — the
  usual cleanup candidates, with absolute `path`
- `largest_files[]`: biggest individual files with their category
- children, special dirs, files and `--category` items also have
  `dedup_bytes`, `freeable_min_bytes` and `clone` (the item is, or holds, a
  file that may share blocks with others)

`--category` returns `{path, category, accounting, items: [{path, size_bytes, size, ...}]}`.

## Shared blocks (APFS clones, hard links)

`size_bytes` is what `du` reports. On macOS, APFS clones (made by uv, bun,
pnpm, `cp -c`, Finder Duplicate) share blocks, so `size_bytes` can be far
more than deleting would free. Clone detection is opt-in because it slows the
scan: on macOS, pass `--clones` (MCP: `clones: true`) whenever you will tell
the user how much a deletion frees. Without it, `freeable_min_bytes` is
`null` on macOS and `accounting.clone_detection` is `"off"`. It is also
`null` on Linux filesystems with reflinks (btrfs, XFS, bcachefs, OCFS2, ZFS)
and with `--cross-fs`, and always on Windows: then no lower bound is known, so do not promise freed
space. Without `--clones`, hard-linked files never count in
`freeable_min_bytes` (it stays a lower bound, just a lower one); `--clones`
also credits a hard-linked file to the folder holding all its links.

- `freeable_min_bytes`: at least this much is freed by deleting the item.
  Use it, not `size_bytes`, when you tell the user how much a cleanup frees.
  It is a lower bound and does not add up across items (two venvs cloned from
  each other each free little alone, both together free one copy).
- `dedup_bytes`: size with each clone family counted once; it adds up like
  `size_bytes`. `clone_shared_bytes = size_bytes - dedup_bytes`.
- `accounting.clone_detection` is `"apfs-full"` when clones were detected,
  `"apfs-partial"` when some files' clone flags could not be read (another
  volume; those count in full and as possibly shared),
  `"unsupported"` when `--clones` was given but the filesystem has no clone
  information (then `dedup_bytes == size_bytes` and only hard links make
  `freeable_min_bytes` smaller), `"off"` without `--clones`. `accounting.caveats` lists what is
  not considered, e.g. APFS snapshots: freed space may appear only after Time
  Machine local snapshots expire.

When `freeable_min_bytes` is much smaller than `size_bytes`, say so ("12 GB
on disk, but its files are clones of your uv cache: deleting it frees at least
1 GB"), and suggest cleaning the shared source (e.g. `uv cache clean`) if the
user wants the space back.

On macOS, `--private` (implies `--clones`) adds `private_bytes` to files that
are partial APFS clones: what deleting that one file frees right now
(`accounting.private` is `true` when collected). Use it before promising space
from deleting cloned files (uv / bun caches, `cp -c` copies). Files older than
an APFS local snapshot report 0 until the snapshot expires. Never add these up
per folder.

## Workflow for "my disk is full"

1. `dutype --json --top 15 ~` (or the path the user names) and summarize the
   top categories and children in a few lines.
2. Drill into the biggest child, or use `--category` for the dominant kind.
3. Propose concrete actions with sizes: regenerable items first (`Build output`,
   `Package cache`, `Python cache`, `node_modules`, `Cache`), then user data.
   Quote `freeable_min_bytes` as the space a deletion frees.
4. Never delete anything without explicit confirmation of the exact paths.
   Prefer the tool's own cleanup where it exists (`cargo clean`,
   `npm cache clean --force`, `uv cache clean`, `docker system prune`).

## MCP

`dutype --mcp` is an MCP server on stdio with two read-only tools,
`disk_usage(path, top?, cross_fs?, refresh?, clones?, size_mode?, private?)` and
`find_by_category(path, category, top?, cross_fs?, refresh?, clones?, size_mode?, private?)`,
with the same fields as `--json`. `size_mode` is `"du"` (default) or `"dedup"`
(implies `clones: true`); `private: true` adds `private_bytes` (implies
`clones: true`).
Claude Code: `claude mcp add dutype -- npx -y dutype --mcp`.
