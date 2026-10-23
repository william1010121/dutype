# TUI refresh navigation validation

## Scope

Successful full refreshes now retain the selected entry at each navigation level,
not just the current folder. The retained `Navigation` contains only the current
path and selected names with their lossiness flags. It does not clone or retain
the old tree. Existing full-scan disposal, cancellation, and cache behavior are
unchanged. On platforms with non-reversible lossy names (including Windows
unpaired UTF-16 surrogates), a rescan cannot prove that even a singleton display
name still identifies the same raw name. Such selections use the ordinary missing
selection fallback, and navigation stops before such a folder. Unix escaped names
remain restorable. Same-tree sorting retains unique lossy names; ambiguous source
or destination matches fall back rather than choosing the first display twin.

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
Successful full refreshes do not require a progress frame: an inert `!` key is
first acknowledged through `--show-keys`, then completion requires the marker to
be cleared by the new App together with the completed header and footer. An old
view cannot immediately pass this check. Five deterministic synchronization tests
exercise completion without progress, unchanged successful views, stale marked
views, progress-only frames, and partially cleared progress frames:

```sh
python3 -m unittest discover -s tests -p test_tui_refresh_history.py -v
```

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
and changes an ancestor's Dedup rank across refresh. Portable synthetic name
fixtures additionally cover non-reversible singleton replacement, old ambiguity
shrinking to a singleton, ambiguous file/folder twins, ancestor/Back fallback, and
unchanged unique-name navigation during same-tree size-mode toggles.

This validation does not change the established policy that cancelling a full
refresh exits. Retaining that old snapshot would change peak-memory behavior and
is outside this change.

## Original execution results

- Rust 1.99.1: format check, all-target Clippy with warnings denied, and all six
  new navigation unit tests passed. Existing subtree/full-scan parity and popup
  refresh-invalidation tests also passed.
- Rust 1.88.0 (MSRV): all six new navigation tests passed.
- A clean build of baseline `2069742` fails the new PTY test at its first
  selected-identity assertion after `R`. A clean build with this change passes
  all seven PTY cases, including both Esc and q cancellation policies.
- The complete stable and Rust 1.88.0 suites subsequently passed: 170 tests
  passed and 2 ignored on each toolchain.

## Review-fix focused validation

- Rust 1.99.1: all ten `refresh_history_*` unit tests passed.
- Five deterministic Python synchronization tests passed without launching a
  process or relying on scan timing.
- All seven isolated real-PTY cases passed with the revised success handshake;
  all four cancellation cases still require an observed progress screen.
- All seven helper failure/cleanup checks passed, including kill-and-wait reaping
  of the owned child after an injected terminate error.
- Final review-fix validation: Rust 1.99.1 format check and all-target Clippy
  with warnings denied passed. Complete Rust 1.99.1 and Rust 1.88.0 suites each
  passed 174 tests, with 2 existing manual benchmarks ignored.
