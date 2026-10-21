<div align="center">

<img src="https://raw.githubusercontent.com/william1010121/dutype/main/assets/logo.svg" width="112" alt="dutype logo">

# dutype

**Disk usage by kind.**<br>
`du` tells you *where* your space went. dutype tells you *what* it is.

[![npm](https://img.shields.io/npm/v/dutype?logo=npm&color=cb3837)](https://www.npmjs.com/package/dutype)
[![GitHub release](https://img.shields.io/github/v/release/william1010121/dutype?logo=github)](https://github.com/william1010121/dutype/releases/latest)
[![CI](https://github.com/william1010121/dutype/actions/workflows/ci.yml/badge.svg)](https://github.com/william1010121/dutype/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**Try it now, nothing to install:**

```bash
npx dutype
```

<img src="https://raw.githubusercontent.com/william1010121/dutype/main/assets/demo.gif" alt="dutype demo" width="900">

</div>

Point dutype at a folder and it tells you, in one screen, how much of it is
**video, PDFs, archives, ML checkpoints, datasets** — and how much is
**`node_modules`, Python virtualenvs, `.git`, build output and caches**. Every
row gets a bar colored by what is inside it, so you can see *why* a folder is
big before you even open it.

## What it does that other tools don't

Great tools like gdu, ncdu, dua and dust answer "which folder is big?".
dutype answers "big *with what*?":

| | **dutype** | gdu | ncdu | dua | dust | diskonaut |
|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| Breakdown by file type (video, PDF, model, …) | ✅ | filter only (`-T`) | — | — | raw extensions (`-t`), non-interactive | — |
| Knows `node_modules`, venvs, `.git`, `target/`, caches as categories | ✅ | — | — | — | — | — |
| Per-row bar showing what each folder is made of | ✅ | — | — | — | — | — |
| "Show me every `node_modules` / venv / cache, with sizes" | ✅ | — | — | — | — | — |
| Your own categories and rules (TOML) | ✅ | — | — | — | — | — |
| JSON output | ✅ | ✅ | ✅ | — | ✅ | — |
| Built-in MCP server for LLM agents | ✅ | — | — | — | — | — |
| Interactive TUI | ✅ | ✅ | ✅ | ✅ | — | ✅ |
| Parallel scan | ✅ | ✅ | opt-in (`-t`) | ✅ | ✅ | ✅ |
| Delete from the UI | not yet | ✅ | ✅ | ✅ | — | ✅ |

And it is the fastest of the bunch in 3 of our 4 benchmarks (see [Performance](#performance)).

### Directory rules, not just extensions

dutype classifies **directories by what they are**, then files by extension:

- `node_modules/`, `bower_components/` → **node_modules**
- any folder containing `pyvenv.cfg` or `conda-meta/` (whatever it is named) → **Python env**
- `target/` containing Cargo's `CACHEDIR.TAG`, `.next/`, `DerivedData/`, … → **Build output**
- `~/.cache/huggingface`, `.ollama/models`, `wandb/`, … → **ML cache**
- `.npm/`, `.gradle/`, `.m2/`, `.cargo/registry`, `~/.cache/pip`, … → **Package cache**
- `.git/`, `.hg/`, `.jj/` → **.git / VCS**, `.Trash/` → **Trash**

Everything inside such a directory counts toward it — a `.js` file in
`node_modules` is `node_modules`, not "Code". Generic caches are *weak*: a more
specific category inside `~/.cache` (like `huggingface/`) still wins. Detection
uses only the directory listing the scan reads anyway, so it costs no extra
syscalls.

<table>
<tr>
<td width="50%"><img src="https://raw.githubusercontent.com/william1010121/dutype/main/assets/screenshot-projects.png" alt="Folder composition"><br><sub><b>What is each project made of?</b> Bars are split by category; the panel on the right breaks the current folder down.</sub></td>
<td width="50%"><img src="https://raw.githubusercontent.com/william1010121/dutype/main/assets/screenshot-popup.png" alt="List every Python env"><br><sub><b>Select a category, press Enter</b>: every Python env (or node_modules, or the biggest PDFs) below this folder, largest first.</sub></td>
</tr>
<tr>
<td width="50%"><img src="https://raw.githubusercontent.com/william1010121/dutype/main/assets/screenshot-extensions.png" alt="Extension mode"><br><sub><b>Press <code>m</code></b> to switch from categories to raw extensions, with file counts.</sub></td>
<td width="50%"><img src="https://raw.githubusercontent.com/william1010121/dutype/main/assets/screenshot-nerd.png" alt="Nerd Font icons"><br><sub><b>Icons</b>: emoji by default, Nerd Font glyphs with <code>--icons nerd</code>, or none.</sub></td>
</tr>
</table>

## Install

**Run without installing** (needs Node.js 16+; downloads a native binary, nothing runs in Node):

```bash
npx dutype                 # current directory
npx dutype ~/Projects      # any directory
npx dutype --report ~      # plain-text report
```

**Install:**

| Method | Command |
|:---|:---|
| npm | `npm install -g dutype` |
| Shell (macOS, Linux) | `curl -LsSf https://github.com/william1010121/dutype/releases/latest/download/dutype-installer.sh \| sh` |
| PowerShell (Windows) | `irm https://github.com/william1010121/dutype/releases/latest/download/dutype-installer.ps1 \| iex` |
| Binaries | [GitHub Releases](https://github.com/william1010121/dutype/releases/latest): macOS (arm64, x64), Linux (x64, arm64, static musl), Windows (x64) |
| From source | `cargo install --git https://github.com/william1010121/dutype` |

Coming soon: `cargo install dutype` (crates.io), `pip install dutype` / `uv tool install dutype` (PyPI) and `brew install william1010121/tap/dutype`.

**Kaggle / Colab / Jupyter**: notebooks have no TTY, so use the report modes.
The shell installer only needs `curl` and puts the binary in `~/.cargo/bin`:

```python
!curl -LsSf https://github.com/william1010121/dutype/releases/latest/download/dutype-installer.sh | sh
!~/.cargo/bin/dutype --report --top 10 /kaggle/working
!~/.cargo/bin/dutype --report --category "ML model" /kaggle/input
```

If the notebook has Node.js, `!npx -y dutype --report /kaggle/working` works too.

## Usage

```bash
npx dutype             # try it without installing
dutype                 # current directory, interactive
dutype ~/Projects      # any directory
dutype --report ~      # plain-text report
dutype --json ~        # JSON report
dutype --json --category node_modules ~   # largest node_modules with sizes (default: 20)
dutype --cache ~       # reuse the last scan of ~ (instant), save new ones
dutype --refresh ~     # rescan and update the disk cache
dutype --json --private ~/.cache/uv   # macOS: also what deleting each partial clone frees
dutype --clones --size-mode dedup /data     # count clones / reflinked extents once
```

With `--cache`, scans are saved in `~/.cache/dutype` (`$XDG_CACHE_HOME/dutype`,
`%LOCALAPPDATA%\dutype\cache` on Windows, or `$DUTYPE_CACHE_DIR`). A cache is
reused until you rescan (`r` for the current folder, `R` for everything) or pass `--refresh`; it is ignored automatically
after a dutype upgrade or a category rule change. dutype never counts its own
cache directory, so totals are the same with or without `--cache`. Snapshots
list every file name, so they are private: the cache directory must belong to
you and is set to mode 0700, snapshots to 0600. `--json` reports `cached_at`
(unix time) when the result came from the cache.

| Key | Action |
|:---|:---|
| `↑` `↓` `PgUp` `PgDn` `g` `G` | move |
| `Enter` `→` | open folder · on a category: list its largest items |
| `←` `Backspace` | go up |
| `Tab` | switch between the folder list and the breakdown |
| `m` | categories ⇄ extensions |
| `c` | sizes: allocated (like `du`) ⇄ deduplicated (APFS clones or shared extents counted once; needs `--clones`) |
| `s` | in a category/extension result popup: jump inside dutype to the directory, or select the file in its parent |
| `o` | reveal in Finder / Explorer / file manager (also `Enter` in a result popup) |
| `y` | copy the selected path to the clipboard (OSC 52 over SSH) |
| `r` | rescan the current folder only (fast; updates the `--cache`) |
| `R` | rescan everything from the root |
| `q` | quit |

Extension breakdown rows use allocated size descending, then display label ascending for ties. This keeps equal-size rows and the top-60 cutoff stable across refreshes and mode switches.

<details>
<summary>All options</summary>

```text
Usage: dutype [OPTIONS] [PATH]

Arguments:
  [PATH]  Directory to analyze [default: .]

Options:
      --report           Print a text report instead of opening the TUI
      --json             Print a JSON report (for scripts and LLM agents)
      --category <NAME>  With --report / --json: only list the largest items of this category
      --top <N>          Number of entries in each list of --report / --json [default: 20]
      --cross-fs         Descend into other filesystems / mount points
      --config <FILE>    Category config file [default: ~/.config/dutype/config.toml]
      --icons <SET>      Icon set for categories [possible values: emoji, nerd, none]
      --dump-config      Print the built-in category rules (a config template) and exit
      --mcp              Run as an MCP server on stdio, for LLM agents
      --cache            Reuse the last scan of this path if there is one, and save new scans (in ~/.cache/dutype, or $DUTYPE_CACHE_DIR)
      --refresh          Ignore a cached scan: scan again and update the cache (implies --cache)
      --clones           Detect shared blocks: APFS clones (macOS) or, on Linux btrfs / XFS / bcachefs, reflinked and snapshot-shared extents. Counts them once (dedup sizes, the TUI's `c` key) and gives a lower bound of what deleting each item frees. Makes scans slower [alias: --reflinks]
      --size-mode <MODE> Size to show and sort by: `du` (allocated, like `du -skx`) or `dedup` (each APFS clone family or shared extent counted once; implies --clones) [default: du]
      --private          Also read the APFS private size of partial clones (macOS only; implies --clones)
```

</details>

### Hard-link attribution

On Unix, hard-linked files count once, just like `du -skx`. Their allocated
bytes belong to the scanned link with the fewest path components, then the
smallest path compared component by component (case-sensitive display names).
If an escaped non-UTF-8 name looks like a valid UTF-8 name, the valid name wins
the tie. Other links have zero allocated bytes. This rule is independent of
scan order and thread count; the chosen link's category and ancestor folders
receive its bytes: a root `app.rs` wins over `node_modules/app.js` and counts
as Code, while `node_modules/a.js` wins over equally deep `z/a.rs` and counts
as node_modules. It does not change the overall count-once total or how
freeable bytes are calculated. Windows still cannot identify hard links.

Stable attribution uses temporary bookkeeping proportional to hard-link entries
and distinct inodes; ordinary-file scans skip it. See the [before/after
benchmark](bench/hardlinks.md) for the measured time and peak-memory tradeoff.

### APFS clones

On APFS (macOS), `cp -c`, Finder's Duplicate, uv, bun and pnpm make *clones*:
files with their own inode that share their blocks with another file. `du`,
and dutype's main number, count every clone in full, so a `.venv` or
`node_modules` cloned from a package cache can look huge while deleting it
frees almost nothing. With `--clones`, dutype detects identical clones while
it scans and adds two numbers next to the `du` size:

- **dedup**: the size with each clone family counted once (on one member,
  picked by a stable rule: the shortest path, then the alphabetically first).
  It adds up like `du` sizes. Press `c` (or pass `--size-mode dedup`, which
  implies `--clones`) to show
  and sort by it; the header then says `clones: −X shared` and rows holding
  clones counted elsewhere get a `⧉`.
- **freeable** (`frees ≥` in the details panel): a lower bound of what
  deleting a folder would free right now: files that share no blocks, plus
  clone families and hard links whose every member is inside the folder.
  It does not add up across folders.

For example, a 21.5 GB `~/.cache/uv` holds 16.8 GB once clones are counted
once, and deleting all of it frees at least 9.4 GB (the rest is shared with
virtualenvs elsewhere).

Clone detection is off by default because it makes warm scans slower: APFS
has to look up every file's clone flags (on an Apple M4, +31% on a 100k-file
Xcode tree, +31% on a 500k-file uv cache, nothing measurable on
`~/.cargo/registry`, which has no clones). Without `--clones`, dedup equals
the `du` size, `accounting.clone_detection` is `"off"`, and on macOS
`freeable_min_bytes` is `null` and the TUI shows `frees ?`: any file may be a
clone, so no lower bound can be promised. On Linux, `freeable_min_bytes` is
also `null` when the scanned root is on a filesystem with reflinks or block
cloning (btrfs, XFS, bcachefs, OCFS2, ZFS; detected once, for the root) and
always with `--cross-fs` (the scan may enter such a filesystem). On ext4 and
other filesystems without shared blocks it is known, but without `--clones`
hard-linked files never count as freeable (finding where all of a file's
links are costs time and memory on link-heavy trees); with `--clones` they
count in the folder that holds all their links. On
Windows it is always `null` (hard links are not identified there). A folder
without files always has a known value (its own blocks). A `--cache` snapshot made without `--clones` is not
reused for a `--clones` run.

Known limitations:

- Only full clones are deduplicated. A clone that was modified afterwards
  (a partial clone) is counted in full and never counted as freeable.
- APFS local snapshots (Time Machine) are not considered: space they still
  reference is freed only once they expire.
- With `--cross-fs`, files on a volume that does not report clone flags are
  counted in full and as possibly shared; `clone_detection` is then
  `"apfs-partial"`.
- Dedup and freeable values are relative to the scanned tree: clone family
  members outside it make a family non-freeable.
- A family's size is read on macOS 14 and later only; on older systems and on
  sealed system volumes, families are deduplicated but never freeable.
- `EF_MAY_SHARE_BLOCKS` (once cloned, maybe still sharing) is sticky and only a
  hint, so freeable can be lower than what deleting really frees.
- With `--cache`, clone families are as they were at scan time: changes
  outside a rescanned folder are not seen until a full rescan (`R`).
- ZFS block cloning and OCFS2 reflinks are not detected: with `--clones` they
  report `clone_detection: "unsupported"`, dedup equals the `du` size and
  `freeable_min_bytes` is `null`. btrfs, XFS and bcachefs: see below.

### Linux reflinks (btrfs, XFS, bcachefs)

`cp --reflink`, deduplication tools and btrfs snapshots share extents between
files the same way. With `--clones` (alias `--reflinks`; MCP: `clones: true`),
when the scan root is on btrfs, XFS (`reflink=1`) or bcachefs, dutype reads
every file's extent map (`FS_IOC_FIEMAP`, one ioctl per file: about 3–6×
slower on a warm cache, still several times faster than `btrfs filesystem du`)
and finds the physical ranges shared with other files. Then:

- **dedup** counts every shared physical byte once, like the "Set shared"
  column of `btrfs filesystem du`. Each shared range stays with one file
  (same rule: the shortest path, then the alphabetically first), so folder
  sizes add up and are the same on every scan. Partial overlaps, such as a
  copy with a few blocks rewritten, are handled.
- **freeable** counts only bytes in no shared extent: Linux keeps no
  reference count that says whether every other user of an extent (a file
  outside the folder, a snapshot) would go too.
- `accounting.clone_detection` is `"fiemap"`, with the caveat
  `snapshots-counted-as-shared` on btrfs: data a snapshot also holds is
  shared, so deleting the live copy frees nothing until the snapshot goes.
- A file that cannot be opened (no read permission) cannot be probed: it
  counts in full and is never freeable, and `clone_detection` becomes
  `"fiemap-partial"` (caveat `shared-extents-missing-for-some-files`).
- Compressed btrfs extents report their uncompressed length, not where
  their bytes are, so shared ones are counted in full (never deduplicated)
  and their files are never freeable (`compressed-extents-not-deduplicated`).

Without `--clones`, sizes on these filesystems are plain `du` sizes,
`clone_detection` is `"off"` and `freeable_min_bytes` is null (any file may
share extents, so no lower bound can be given; the same holds on OCFS2 and
ZFS, and for any Linux scan with `--cross-fs`). ext4 and other filesystems
without shared blocks report numbers either way (`"unsupported"` with
`--clones`). The filesystem is detected once, at the scan root; with
`--cross-fs` the scan may enter filesystems that cannot be probed, so shared
extents are not read at all (dedup equals the `du` size).

## Categories

24 built-in categories:

| Directories | | File types | |
|:---|:---|:---|:---|
| 📦 node_modules | 🐍 Python env | 🎬 Video | 🎨 Image |
| 🧹 Python cache | 🧠 ML cache | 🎵 Audio | 📕 PDF |
| 🌿 .git / VCS | 🔨 Build output | 📝 Document | 🧳 Archive |
| 📥 Package cache | 💾 Cache | 💿 Installer/disk | 🤖 ML model |
| 🚮 Trash | | 📊 Data | 🔤 Font |
| | | 📜 Log | ⏳ Temp/backup |
| | | 🧩 Binary | 💻 Code |

`dutype --dump-config` prints every rule.

### Make it yours

Add rules in `~/.config/dutype/config.toml` (or pass `--config`). A
`[[category]]` with an existing name extends it; a new name creates a category.

```toml
icons = "nerd"            # emoji (default) | nerd | none

[[category]]              # extend a built-in category
name = "Video"
extensions = ["braw", "r3d"]

[[category]]              # add your own
name = "Unity"
icon = "🎮"
color = "#57b9ff"         # or a 256-color index like 75
dirs = ["Library", "Temp"]            # folder names; "parent/name" also works
markers = ["ProjectSettings"]         # a folder containing this entry
extensions = ["unitypackage"]
```

Directory rules beat extension rules, and your rules beat the built-in ones.

## For LLM agents

dutype is built to be called by agents as well as people.

**JSON** — `dutype --json PATH` returns totals, every category's share, the
biggest children, the biggest special directories (the usual cleanup
candidates) and the biggest files, with absolute paths and byte sizes (here
`dutype --json --clones ~` on a Mac):

```json
{
  "path": "/Users/me",
  "size_bytes": 190982119424,
  "dedup_bytes": 186126925824,
  "clone_shared_bytes": 4855193600,
  "freeable_min_bytes": 171798691840,
  "accounting": { "size": "du-allocated", "size_mode": "du", "clone_detection": "apfs-full",
                  "caveats": ["partial-clones-not-deduplicated", "snapshots-not-considered"] },
  "files": 666884,
  "categories": [{ "name": "Data", "size_bytes": 87828418560, "size": "81.8 GiB", "percent": 46.0 }, …],
  "children": [{ "name": "dataset", "type": "dir", "percent": 27.6, "top_categories": […],
                 "dedup_bytes": 52707115008, "freeable_min_bytes": 52707115008, "clone": false }, …],
  "special_dirs": [{ "path": "/Users/me/proj/.venv", "category": "Python env", "size": "7.8 GiB" }, …],
  "largest_files": [{ "path": "/Users/me/ckpt/last.safetensors", "category": "ML model", "size": "12.0 GiB" }, …]
}
```

`size_bytes` is always the `du` size. With `--clones`, `dedup_bytes` counts
each APFS clone family (or, on Linux, each shared extent) once and
`freeable_min_bytes` is a lower bound of what deleting the item frees (see
[APFS clones](#apfs-clones) and
[Linux reflinks](#linux-reflinks-btrfs-xfs-bcachefs); without `--clones` it
is `null` on macOS and on btrfs / XFS / bcachefs); children, special dirs, files and
`--category` items carry the same fields plus `clone` (the item is, or holds,
a file that may share blocks). `--size-mode dedup` ranks the lists by
`dedup_bytes`.

Full and category JSON reports (including MCP `disk_usage` and
`find_by_category`) include `unreadable`: the scan-wide count of entries that
could not be read, not a per-category count, since unreadable entries may be
unclassifiable. An empty category list with a nonzero count may be incomplete.
`cached_at` is `null` for a fresh scan; otherwise it is the saved or
session-retained snapshot's Unix timestamp, not a guarantee of current
filesystem state. Both text report forms show cached-snapshot and unreadable
notices when applicable. A cached partial scan keeps its unreadable count
even after permissions change; use `--refresh` (MCP: `refresh: true`) to rescan.

Category JSON (`--category` and MCP `find_by_category`) also includes:

- `matched_items` (unsigned integer): matching items below the root in the
  scanned snapshot, before the top-N limit. A matching directory counts once
  and its descendants are not searched; zero-byte matches count too.
- `items_truncated` (boolean): `matched_items > items.length`. This describes
  list truncation only, not scan completeness: unreadable entries are not
  inferred or counted as matches. Cached counts describe the cached snapshot.

Category text shows, for example, `1 of 4 shown` and a `displayed total` for
only the returned items, using the selected size mode, not all matches.
CLI `--top 0` shows no items but still counts matches; MCP keeps its existing
`top` range of 1–500. Larger CLI limits do not reserve memory for the entire
requested limit up front, and do not impose a new result cap.

**Private sizes (`--private`, macOS/APFS)** — a file that was cloned and then
changed (a *partial clone*) shares only some blocks with other files, so neither
`size_bytes` nor the clone-family numbers say what deleting it frees. With
`--private` (MCP: `private: true`; both imply `--clones`, whose flags pick out
partial clones), every file that may share blocks (`EF_MAY_SHARE_BLOCKS`) but is
not a full clone (`EF_SHARES_ALL_BLOCKS`), and has a single link, gets
`private_bytes`: the bytes freed immediately if that file alone is deleted (APFS
`ATTR_CMNEXT_PRIVATESIZE`). It appears on file entries (`largest_files`,
`--category` items, file `children`) and in the TUI details pane; it is omitted
on other files (and on one that vanished or could not be read meanwhile) and
never summed into folder values. `accounting.private` is `true` when the scan
read private sizes for every partial clone (macOS volumes that report APFS clone
flags and private sizes); it is `false` if any file's clone flags or private
size could not be read. APFS local snapshots (e.g. Time Machine) keep older
blocks, so files older than the latest snapshot often report 0. Reading it costs
one `getattrlistat` per partial clone: about +4% over `--clones` alone on
Xcode's `Platforms` (108k files) and +16% on a uv cache (502k files, 18k partial
clones); most of the cost over a plain scan is `--clones` itself. Elsewhere
`--private` is accepted, `accounting.private` is `false` and no `private_bytes`
are reported. A `--cache` snapshot made with `--private` keeps the values (and
also serves runs without it); one made without it is not used for a `--private`
run.

**MCP server** — `dutype --mcp` exposes two read-only tools,
`disk_usage(path, top?, cross_fs?, refresh?, clones?, size_mode?, private?)` and
`find_by_category(path, category, top?, cross_fs?, refresh?, clones?, size_mode?, private?, paginate?, cursor?)`
(`clones: true`, `size_mode: "dedup"` or `private: true` turns on clone detection).
They never write files.
With `dutype --mcp --cache`, repeated calls on the same path reuse the scan from
memory (or a snapshot the CLI saved in `~/.cache/dutype`) instead of walking the
disk again. `refresh: true` rescans and replaces only that session's in-memory
snapshot; MCP never writes or replaces the CLI's disk cache. A new MCP session
can still load the old disk snapshot until a CLI run with `--refresh` updates it.


### Sequential category pages

For an occasional complete category inventory beyond 500 matches, start the
server with `dutype --mcp --cache` (without server-wide `--refresh`) and opt in:

```json
{"path":"/data/projects","category":"node_modules","top":500,"paginate":true}
```

The result keeps every existing category field and adds:

```json
{"pagination":{"snapshot_id":"opaque-generation-id","offset":0,"has_more":true,"next_cursor":"opaque-token"}}
```

For the next page, repeat the exact absolute `path` returned by the first page,
the category, and effective query options, and pass `cursor: next_cursor`.
Cursor presence implies pagination; `top` may change between pages (1–500,
default 50). Omitted options still use their usual defaults, so repeat any
nondefault `size_mode`, `cross_fs`, `clones`, and `private` settings. Category
aliases are equivalent; `dedup` or `private: true` still imply clones. A cursor
cannot be combined with `refresh: true` or `paginate: false`.

- `snapshot_id` identifies one retained tree generation, not its timestamp.
  `offset` is how many matches precede this page in the category's ranking.
- Stop when `has_more` is false and `next_cursor` is null. For 600 matches,
  pages of 500 and 100 both have `items_truncated: true`: that field still means
  `matched_items > items.length`, not that another page exists. Empty categories
  have no cursor. `unreadable` remains the same scan-wide warning on every page.
- Ordering is selected size descending, then relative path descending. Only
  exact size/path collisions use a snapshot-local visit ordinal, so lossy
  displayed names do not cause matches to disappear between pages.
- Pages describe one completed scan, not an atomic filesystem snapshot or
  current deletion savings. Filesystem edits, root removal and CLI disk-cache
  rewrites do not change its results. Continuations perform no filesystem scan,
  path canonicalization, sharing probe or clone/private I/O.
- A successful refresh through either MCP tool, or replacement to satisfy a
  more demanding clone/private request for that same root/filesystem policy,
  invalidates previous cursors. Failed refreshes retain the previous tree;
  requests on other roots do not invalidate it. Restarting the server always
  requires a new cursor, including when it reloads the same disk snapshot.
  Stale cursors return `isError: true` with structured
  `{"code":"stale_cursor","restart_required":true,"message":"..."}`. Start
  again without a cursor; add `refresh: true` to that first request for new data.
  Invalid cursor formats and changed queries are JSON-RPC `-32602` errors.
- No cursor means no pagination unless `paginate: true`; legacy defaults and
  output remain unchanged. Pagination in non-cache or server-wide refresh mode
  is an execution error. MCP never persists cursor state or writes disk caches.

The cache keeps one current tree per root/filesystem policy for the session;
old generations are not pinned by cursors and there is no per-cursor registry.
Each page retains at most 500 candidates, constructs only that page's entries,
and makes one tree traversal initially or at most two on continuation. Deep
traversal is stack-safe. Enumerating many small pages trades repeated tree
walks for bounded page memory; it is not an indexed database listing. The
existing session cache's total memory across different roots is not capped.

Add the server to an MCP client:

```bash
claude mcp add dutype -- npx -y dutype --mcp      # Claude Code
codex mcp add dutype -- npx -y dutype --mcp       # Codex CLI
```

```json
{ "mcpServers": { "dutype": { "command": "npx", "args": ["-y", "dutype", "--mcp"] } } }
```

(Claude Desktop, Cursor and other MCP clients.) With dutype installed, use
`dutype --mcp` as the command instead of `npx -y dutype --mcp`.

**Agent skill** — [`skills/dutype/SKILL.md`](skills/dutype/SKILL.md) teaches an
agent the "my disk is full" workflow. For Claude Code:

```bash
mkdir -p ~/.claude/skills/dutype && curl -fsSL -o ~/.claude/skills/dutype/SKILL.md \
  https://raw.githubusercontent.com/william1010121/dutype/main/skills/dutype/SKILL.md
```

There is also an [`llms.txt`](llms.txt).

## Performance

Mean wall time, lower is better. dutype also classifies every file while it
scans; the others only sum sizes.

| | `~` warm | `~` cold | `/usr` warm | `/usr` cold |
|:---|---:|---:|---:|---:|
| **dutype** 0.1.0 | **304 ms** | 1.02 s | **79 ms** | **300 ms** |
| gdu 5.38.0 | 393 ms | **0.93 s** ± 0.11 | 114 ms | 382 ms |
| dua-cli 2.45.1 | 476 ms | 1.07 s | 128 ms | 308 ms |
| dust 1.2.6 | 639 ms | 1.33 s | 148 ms | 371 ms |
| ncdu 2.9.1 `-t 16` | 610 ms | 1.29 s | 154 ms | 380 ms |
| ncdu 2.9.1 (default, 1 thread) | 3,237 ms | 13.55 s | 901 ms | 3,938 ms |
| GNU du 9.4 | 2,056 ms | 12.65 s | 558 ms | 3,660 ms |

- **Warm cache** (repeat scans): dutype is 1.3–2× faster than the other parallel tools.
- **Cold cache** (first scan after boot): everything is bound by the SSD. dutype,
  dua and gdu are within run-to-run noise of each other on `~`; dutype is
  fastest on `/usr`.

CPU time and peak memory (warm, mean of 5 runs, GNU time):

| | `~` CPU | `~` peak RSS | `/usr` CPU | `/usr` peak RSS |
|:---|---:|---:|---:|---:|
| **dutype** | **3.46 s** | 117 MiB | **0.90 s** | 35 MiB |
| gdu | 4.62 s | 99 MiB | 1.26 s | 34 MiB |
| dua-cli | 5.52 s | **27 MiB** | 1.32 s | **8 MiB** |
| dust | 6.88 s | 245 MiB | 1.59 s | 59 MiB |
| ncdu `-t 16` † | 2.76 s | 49 MiB | 0.65 s | 9 MiB |
| GNU du (1 thread) | 2.05 s | 10 MiB | 0.55 s | 5 MiB |

dutype uses the least CPU of the parallel tools. It keeps the whole tree in
memory (so the TUI can browse it without rescanning) plus a per-directory
category breakdown, which costs more RAM than dua's streaming aggregate.
† ncdu runs in export mode (`-o /dev/null`), which streams instead of keeping
a tree, so its memory is not comparable.

<details>
<summary>Setup and methodology</summary>

- AMD Ryzen 7 5800HS (8 cores / 16 threads), 16 GB RAM, NVMe SSD (SK hynix
  HFM512GD3JX013N), ext4, Ubuntu 24.04, Linux 7.0.
- `~`: 177.9 GiB in 666,884 files (large datasets and checkpoints).
  `/usr`: 7.8 GiB in 192,602 files (many small files).
- [hyperfine](https://github.com/sharkdp/hyperfine) 2.0.0. Warm: 2 warm-up
  runs + 10 measured. Cold: 5 runs, `sync; echo 3 > /proc/sys/vm/drop_caches`
  before each.
- Every tool stays on one filesystem and runs non-interactively:
  `dutype --report`, `dua -x aggregate`, `gdu -npsx`, `dust -x -d 0 -P`,
  `ncdu -x -0 -o /dev/null`, `du -skx`. Totals agree across tools.
- No tuning: dutype uses one thread per logical CPU, like everywhere else.
- Scripts and raw results: [`bench/`](bench/).

</details>

### How it is fast

- Directories are scanned in parallel with [rayon](https://github.com/rayon-rs/rayon)'s
  work stealing; files in a directory are stat'ed once with `lstat` semantics.
- Classification needs no extra I/O: directory rules look at names the scan
  already has; extension lookups are a lowercase-in-a-stack-buffer hash probe.
- Hot paths avoid allocation and shared state: progress counters are updated
  once per directory, top-N lists only build a path when an item makes the cut,
  and the tree is not freed one node at a time on exit.

## License

[MIT](LICENSE)
