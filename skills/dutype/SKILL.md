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
dutype --json --category node_modules PATH   # largest node_modules with sizes (default: 20)
dutype --json --category "Python env" ~      # largest venvs / conda envs (default: 20)
dutype --json --category Video ~/Movies      # biggest video files
dutype --json --category Unity --include-files PATH # custom mixed directory/file category
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
- On Linux btrfs / XFS / bcachefs, `--clones` (slower: one ioctl per file)
  makes `dedup_bytes` count reflinked copies and snapshot-shared data once
  (`accounting.clone_detection: "fiemap"`). Shared extents are never in
  `freeable_min_bytes`; on btrfs, data a snapshot also holds is not freed
  until the snapshot is deleted (`snapshots-counted-as-shared`). Without
  `--clones`, `freeable_min_bytes` is null there.

`--category` returns `{path, category, cached_at, unreadable, accounting,
matched_items, items_truncated, items: [{path, size_bytes, size, ...}]}`.
`matched_items` counts matches below the root in the scanned/cached snapshot
before the top-N limit; a matching directory counts once and prunes descendants.
`items_truncated` means `matched_items > items.length`, not that the scan is
complete: `unreadable` is scan-wide and unclassifiable entries are not inferred.
Text totals cover only displayed items. CLI `--top 0` still counts matches;
MCP `top` remains 1–500.

Directory categories list matching directories by default. For a custom mixed
category (for example Unity with `Library` directories and `unitypackage`
files), add `--include-files` or MCP `include_files: true`. A file must match
both the scanned category and direct filename classification under final rules.
Matching directories still prune descendants regardless of the top-N cutoff.
The root is excluded; explicitly matching files below an excluded special root
can appear, but arbitrary inherited files cannot. Other's filename fallback is
included when Other has directory rules. Pure-category matches stay unchanged.
True is echoed in category JSON; false/omitted preserves the default payload.
CLI requires `--category` and `--report` or `--json`, rejects `--mcp` and
`--dump-config`, and never changes the TUI. MCP accepts the option only on
`find_by_category`. Inventory totals need not equal category summary totals.

For a category inventory, increase CLI `--top` to at least `matched_items`
and check that `items_truncated` is false in the result. This lists all matches
in that snapshot, not a guarantee that unreadable entries were classified.

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
`find_by_category(path, category, top?, cross_fs?, refresh?, clones?, size_mode?, private?, include_files?, paginate?, cursor?)`,
with the same fields as `--json`. `size_mode` is `"du"` (default) or `"dedup"`
(implies `clones: true`); `private: true` adds `private_bytes` (implies
`clones: true`).
With `dutype --mcp --cache`, `refresh: true` replaces the session's in-memory
snapshot only; MCP never writes or replaces the CLI's disk cache. Use a CLI
run with `--refresh` to update the disk snapshot used by future sessions.
Claude Code: `claude mcp add dutype -- npx -y dutype --mcp`.


For category inventories beyond 500 matches, use `dutype --mcp --cache` without
server-wide `--refresh`. Start with `paginate: true`; follow
`pagination.next_cursor`, repeating the returned absolute `path`, category and
effective options, including `include_files` (changing it rejects the cursor,
even for pure categories). `top` remains 1–500 (default 50) and may change between pages.
Cursor presence implies pagination; never combine it with `refresh: true` or
`paginate: false`. Stop on `pagination.has_more: false` / `next_cursor: null`,
not `items_truncated`, which still compares all matches to this page's length.
Pages keep one completed scan's counts, hard-link attribution, accounting and
scan-wide `unreadable` warnings, not atomic/live filesystem truth. Continuation
works even if the root disappears and does not rescan. Successful refresh via
either tool, capability replacement or server restart invalidates old cursors;
failed refresh and other roots do not. On structured `stale_cursor` with
`restart_required: true`, start again without a cursor (optionally refresh).
One current tree per root/filesystem policy remains in session memory; cursors
never pin old trees or persist to disk. Each page selects at most 500 candidates
in at most two tree walks; many small pages repeat that work. Without opt-in,
legacy responses/defaults remain unchanged.
