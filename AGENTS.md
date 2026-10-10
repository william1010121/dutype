# AGENTS.md

dutype is a Rust disk usage analyzer that attributes every byte to a category
(file types and special directories such as node_modules or Python envs). It
has a ratatui TUI, `--report` / `--json` output and an MCP server (`--mcp`).

## Layout

- `src/scan.rs`: parallel directory scan (rayon), tree, top-N helpers. Hot path.
- `src/category.rs`: category registry built from `src/default_config.toml`
  plus the user's `~/.config/dutype/config.toml`.
- `src/report.rs`: text and JSON reports, `find` by category.
- `src/cache.rs`: `--cache` snapshots of a scan in `~/.cache/dutype`, keyed
  by path and invalidated by the category rules' fingerprint.
- `src/mcp.rs`: MCP server over stdio (JSON-RPC, one message per line).
- `src/app.rs`, `src/ui.rs`: TUI state and rendering.
- `bench/`: benchmark scripts and the results quoted in the README.

## Checks

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs these on Linux, macOS and Windows. On this macOS machine the default
`cc` is GCC from Nix; build with `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/clang`.

## Releases

Push a `vX.Y.Z` tag matching `Cargo.toml`; `.github/workflows/release.yml`
(cargo-dist) builds every target, creates the GitHub Release and publishes to
npm with trusted publishing (OIDC, no token). The `publish-npm` job is
hand-edited (`allow-dirty = ["ci"]`): if you run `dist generate`, keep its
`id-token: write` permission, Node 24 / latest npm and `--provenance`.

## Code Review Rules

### Scan performance

- Nothing in `scan_dir` may add a syscall, lock or heap allocation per
  directory entry. Classification must use names the scan already read.
- Progress counters are updated once per directory, never per file.
- Top-N collection must check `TopN::accepts` before building a `PathBuf`.
- Changes to `scan.rs` or `category.rs` lookups need a before/after number
  from `bench/run.sh` or hyperfine in the PR description.

### Correctness

- Sizes are allocated bytes (`blocks * 512` on unix) and hard links count once;
  totals must keep matching `du -skx`, except that dutype's own cache
  directory is always skipped (so `--cache` never changes the totals).
- Directory rules beat extension rules; inside a non-weak special directory,
  every file inherits its category.
- `Other` must stay the last category; inserting a category must renumber all
  rule maps (see `Registry::shift_from`).
- Platform-specific code goes through the `platform` module so Windows builds.

### Interfaces agents depend on

- `--json` and MCP output fields are a public API: do not rename or remove
  fields, only add. Paths are absolute, sizes are in `size_bytes`.
- MCP tools are read-only. Never add a tool that deletes or modifies files.
- `--report` / `--json` must never open the TUI or wait for input, and must not
  panic when stdout is a closed pipe.
