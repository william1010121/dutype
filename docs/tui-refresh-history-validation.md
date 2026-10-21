# TUI refresh navigation validation

## Scope

Successful full refreshes now retain the selected entry at each navigation level,
not just the current folder. The retained `Navigation` contains only the current
path and selected names with their lossiness flags. It does not clone or retain
the old tree. Existing full-scan disposal, cancellation, and cache behavior are
unchanged.

At baseline `2069742`, a real PTY selected `z_selected.pdf` under `branch/inner`.
After `R`, the folder remained `branch/inner` but the first file, `a_big.pdf`, was
selected instead. The same fixture now retains `z_selected.pdf`, including after
its allocated size moves it to a different row.

## Regression checks

Run the bounded shallow PTY regression on Linux:

```sh
cargo build
python3 tests/tui_refresh_history.py --binary target/debug/dutype --output /tmp/dutype-refresh-proof
```

The script creates only temporary synthetic fixtures, isolates HOME/config/cache,
sets PATH to `/no-external-commands`, and sends no reveal, file-opening, or
clipboard commands. The tested binary is invoked by its absolute path.
It observes the progress screen before sending cancellation, rather than assuming
a scan is active after a sleep. A flat 8,192-file fixture provides an observable
scan; no deep hierarchy is involved. Each wait has a deadline, each child has
cleanup, and all cases verify normal process exit and restored terminal settings.

Cases cover successful repeated `R`, selection reordering, Back history,
lowercase `r` at root, nested lowercase `r`, removed selection/folder fallback,
and both Esc and q during nested folder-only and full scans. Folder-only aborts
must render the unchanged old selection and `rescan cancelled`; full aborts must
exit without rendering a completed replacement snapshot.

Unit tests named `refresh_history_*` cover the same navigation identity rules,
including an empty folder, root selection staying nonfirst, renamed selections,
folder-to-file replacement, Unicode selected directories, raw/literal display-name
collisions in both directory and selected-file steps, popup invalidation, and
unchanged r/R routing. A clone fixture makes Du and Dedup orders genuinely differ
and changes an ancestor's Dedup rank across refresh.

This validation does not change the established policy that cancelling a full
refresh exits. Retaining that old snapshot would change peak-memory behavior and
is outside this change.

## Execution results

- Rust 1.99.1: format check, all-target Clippy with warnings denied, and all six
  new navigation unit tests passed. Existing subtree/full-scan parity and popup
  refresh-invalidation tests also passed.
- Rust 1.88.0 (MSRV): all six new navigation tests passed.
- A clean build of baseline `2069742` fails the new PTY test at its first
  selected-identity assertion after `R`. A clean build with this change passes
  all seven PTY cases, including both Esc and q cancellation policies.
- The complete stable and Rust 1.88.0 suites subsequently passed: 170 tests
  passed and 2 ignored on each toolchain.
