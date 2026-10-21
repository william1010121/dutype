# Deep-tree TUI checks

The default Rust suite contains one filesystem-free 1000-directory regression,
`app::tests::deep_popup_jump_and_size_modes_preserve_snapshot_on_small_stack`.
It runs on a 128 KiB thread stack and covers extension aggregation, category
and extension popups, matching-directory pruning, captured-index navigation,
size-mode reordering, shared-record index remapping, and destruction. Clone
accounting is supplied as a synthetic completed snapshot; the test checks its
preservation, not the accounting algorithm or a live APFS/reflink filesystem.

An optional Linux PTY check exercises a real temporary 1000-directory fixture:

```sh
cargo build
python3 tests/tui_deep_tree.py --binary target/debug/dutype --output /tmp/dutype-deep-proof
```

Use `--depth 350` for the smaller finite fixture. The helper imports only the
VT screen decoder from `tui_result_jump.py`; it has no Python package dependencies.
It creates one 8 MiB PDF, tests both category and extension result jumps, resizes
to 1×1 and back, toggles du/dedup sizes, checks Back selection, and verifies clean
terminal restoration. It isolates PATH/config/home, never sends reveal/open/copy
keys, reaps only its own child processes, and removes tracked fixture paths in
reverse order rather than recursively. ANSI output and decoded frames are saved
for diagnosis even if a session fails.

Completion is condition-based with bounded failure timeouts, not a performance
threshold. Linux inotify verifies that result jumps do not open/read fixture
paths; metadata-only syscalls are not measured. Ordinary files do not establish
actual shared-extent/clone behavior, which the synthetic Rust test covers only
at the snapshot-state level. The PTY helper is intentionally not in default CI.
