<div align="center">

<img src="https://raw.githubusercontent.com/william1010121/dutype/main/assets/logo.svg" width="112" alt="dutype logo">

# dutype

**Disk usage by kind.**<br>
`du` tells you *where* your space went. dutype tells you *what* it is.

[![crates.io](https://img.shields.io/crates/v/dutype?logo=rust&color=e67e22)](https://crates.io/crates/dutype)
[![npm](https://img.shields.io/npm/v/dutype?logo=npm&color=cb3837)](https://www.npmjs.com/package/dutype)
[![PyPI](https://img.shields.io/pypi/v/dutype?logo=pypi&logoColor=white&color=3775a9)](https://pypi.org/project/dutype/)
[![CI](https://github.com/william1010121/dutype/actions/workflows/ci.yml/badge.svg)](https://github.com/william1010121/dutype/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

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

| Package manager | Command |
|:---|:---|
| Cargo | `cargo install dutype` &nbsp;·&nbsp; prebuilt: `cargo binstall dutype` |
| npm | `npm install -g dutype` &nbsp;·&nbsp; or run once: `npx dutype` |
| pip / uv / pipx | `pip install dutype` &nbsp;·&nbsp; `uv tool install dutype` &nbsp;·&nbsp; `pipx install dutype` |
| Homebrew | `brew install william1010121/tap/dutype` |
| Shell (macOS, Linux) | `curl -LsSf https://github.com/william1010121/dutype/releases/latest/download/dutype-installer.sh \| sh` |
| PowerShell (Windows) | `irm https://github.com/william1010121/dutype/releases/latest/download/dutype-installer.ps1 \| iex` |
| Binaries | [GitHub Releases](https://github.com/william1010121/dutype/releases) for macOS (arm64, x64), Linux (x64, arm64, static musl) and Windows |

**Kaggle / Colab / Jupyter**: `pip install` gives you a native binary, no
Rust needed. Notebooks have no TTY, so use the report modes:

```python
!pip install -q dutype
!dutype --report --top 10 /kaggle/working
!dutype --report --category "ML model" /kaggle/input
```

## Usage

```bash
dutype                 # current directory, interactive
dutype ~/Projects      # any directory
dutype --report ~      # plain-text report
dutype --json ~        # JSON report
dutype --json --category node_modules ~   # every node_modules with its size
```

| Key | Action |
|:---|:---|
| `↑` `↓` `PgUp` `PgDn` `g` `G` | move |
| `Enter` `→` | open folder · on a category: list its largest items |
| `←` `Backspace` | go up |
| `Tab` | switch between the folder list and the breakdown |
| `m` | categories ⇄ extensions |
| `o` | reveal in Finder / Explorer / file manager |
| `q` | quit |

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
```

</details>

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
candidates) and the biggest files, with absolute paths and byte sizes:

```json
{
  "path": "/home/me",
  "size_bytes": 190982119424,
  "files": 666884,
  "categories": [{ "name": "Data", "size_bytes": 87828418560, "size": "81.8 GiB", "percent": 46.0 }, …],
  "children": [{ "name": "dataset", "type": "dir", "percent": 27.6, "top_categories": […] }, …],
  "special_dirs": [{ "path": "/home/me/proj/.venv", "category": "Python env", "size": "7.8 GiB" }, …],
  "largest_files": [{ "path": "/home/me/ckpt/last.safetensors", "category": "ML model", "size": "12.0 GiB" }, …]
}
```

**MCP server** — `dutype --mcp` exposes two read-only tools,
`disk_usage(path, top?, cross_fs?)` and `find_by_category(path, category, top?)`:

```bash
claude mcp add dutype -- dutype --mcp      # Claude Code
codex mcp add dutype -- dutype --mcp       # Codex CLI
```

```json
{ "mcpServers": { "dutype": { "command": "dutype", "args": ["--mcp"] } } }
```

(Claude Desktop, Cursor and other MCP clients.)

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
