# Result-jump validation

The category/extension popup's `s` key jumps inside dutype: directories are
entered; files are selected in their parent. Enter/o still reveal externally,
y still copies, and Left/Backspace retain normal parent navigation. Captured
child-index chains identify results even when raw names have identical display
paths. Refresh, re-sort, and mutable tree access invalidate popup results.

## Reproduce

Run the normal checks, plus the supported minimum Rust version:

```sh
cargo +stable fmt --check
cargo +stable clippy --all-targets -- -D warnings
cargo +stable test
cargo +1.88.0 test
```

The nine `result_jump` unit/render tests cover nested and direct files,
special/empty directories, zero-byte ties and the top-500 bound, raw/display-name
collisions, missing selections, invalid chains, actual dedup re-ordering,
refresh/re-sort invalidation, repeated navigation, Back, cancellation, and tiny
viewports. External-action dispatch tests use unavailable filesystem paths, so
no reveal, copy, or file-opening action runs.

For actual terminal interaction on Linux, using only Python's standard library
and libc inotify:

```sh
cargo +stable build
python3 tests/tui_result_jump.py --binary target/debug/dutype --output /tmp/dutype-tui-proof
```

The harness creates synthetic temporary fixtures, records raw ANSI output and
reconstructed screens, and checks the selection marker within the correct pane.
It runs five fresh processes and 13 jumps: du/dedup, category/extension,
nested/direct files, a zero-byte file, directory entry, raw-name collisions,
repeated navigation, cancellation, refresh after rename, and empty-root exit.
It resizes a live popup from 140×36 to 40×10, then 1×1 and back, before jumping
and navigating Back. Every exit must restore termios and leave the alternate
screen. No external reveal, clipboard, or file-opening key is sent in a popup.

## Verified results

On Linux, against base `f0ac49f1482cdfbda2418f3add5f46541dc4c419`, stable fmt/clippy and
both full test suites passed: 149 passed and 2 existing ignored tests per
suite. The nine focused tests and the five-process/13-jump PTY run also passed
an independent Linux rerun. Tiny-render testing found an existing zero-width bar
clamp panic; the minimal width guard is covered by the tests.

Inotify observed 2–32 open/access events during each initial scan and zero
such events during every jump. This detects file/directory opens and accesses,
not metadata-only syscalls, and is not comprehensive syscall tracing. A single
strace attempt was unavailable because the environment returned kernel EPERM
for ptrace; it was not retried. Source inspection confirms the jump itself
uses captured tree indices and an in-memory breakdown refresh, with no rescan
or filesystem path resolution.

## Windows collision-fixture correction

The first Windows CI run failed the synthetic collision test, before navigation:
it used Unix-style `d\xffir` / `f\xff.pdf` display names on Windows. Each backslash
is a Windows path separator, so these impossible Windows entry names added two
components per traversal push while pop removed only one. The resulting display
paths therefore differed. Production Windows lossy-name rendering uses U+FFFD
for unpaired surrogates, not Unix backslash escapes; real entry names cannot
contain separators.

The test remains enabled on every platform and still checks all four colliding
node identities in both category and extension mode. It now uses the platform's
rendering shape (Unix escapes or non-Unix U+FFFD) and asserts that every fixture
name has exactly one path component. Production code is unchanged. Local Linux
stable fmt/clippy/full tests and Rust 1.88 full tests were rerun after this
correction. No native Windows runtime was available locally; Windows acceptance
requires the PR's Windows CI rather than inference from the Linux tests.
