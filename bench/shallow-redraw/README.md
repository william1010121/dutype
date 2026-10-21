# Bounded shallow TUI redraw benchmark

This Linux-only research harness compares the pinned baseline renderer at
`d4cd241fa981c3d7074c1a6c396ca345af5861f7` with the checkout's renderer, using
otherwise identical production modules. It creates only a temporary build copy;
production Cargo.toml and dependencies stay unchanged. The pinned commit must be
available locally. Rust 1.99.1 and the locked Cargo dependencies must be cached.

From this directory, with cargo on PATH and CARGO_HOME/RUSTUP_HOME configured:

    ./run.sh verify
    ./run.sh bench
    ./run.sh bench-small
    ./run.sh bench-large

Builds are offline and locked. The emitted temporary build path is retained.
CARGO_TARGET_DIR may point to an existing absolute disposable build cache.
Verification and benchmarks are explicit commands; none modifies scheduling,
affinity, system settings, user configuration, or real user files.

## Fixture and measurement

The largest fixture has exactly 10,000 direct file children in memory, with eight
extensions and deterministic varied sizes. It does not scan disk. Smaller
controls use 0, 1, 24, and 100 files. The ordinary terminal is 120×40; resize cases
alternate real TestBackend dimensions between 120×40 and 100×30. The tiny control
uses 4×7. Navigation cycles Down/PageDown/End/Up/Home/PageUp.

Eight warmup frames precede five batches. Baseline/candidate order alternates.
`bench` and `bench-large` use 40 frames/batch; `bench-small` uses 400. Allocation
tallies are disabled while timing, although the common global allocator retains
a boolean check. Separate four-frame runs count allocations, reallocations,
requested byte traffic, peak incremental live requested bytes, and final live
byte delta. These are not process RSS or allocator-reserved memory. Resize cases
include backend/terminal resizing.

## Bounded results

Final 10,000-file medians on Linux x86_64 / AMD EPYC 9V74, release Rust 1.99.1:

| Case | Baseline ms/frame | Candidate ms/frame |
|---|---:|---:|
| Unchanged forced redraw | 9.471 | 0.354 |
| Navigation | 9.438 | 0.360 |
| Resize | 9.946 | 0.373 |
| Tiny view | 9.348 | 0.083 |

At 120×40, entry-name constructions fall from 10,001 to 33 and category-bar
apportion calls from 10,002 to 34. Unchanged redraw allocations/frame fall from
200,224 to 1,066; reallocations from 29,093 to 135. Requested traffic/frame falls
from 14,235,013 to 724,530 bytes. Peak incremental live requested bytes over the
measurement window fall from 9,322,227 to 668,560; final live delta is zero for
both. Cheap full-N row slots and the sizes vector remain: total draw is still
O(N), not O(visible).

Small directories are not guaranteed faster. In the longer control, one-entry
unchanged/navigation/resize medians were 157.4/138.8/155.4 µs before versus
149.0/140.1/153.4 µs after. The 24-entry values were 254.5/267.5/287.5 µs versus
251.9/277.4/293.3 µs. Initial shorter runs also showed small absolute regressions;
summary.csv retains all measured medians rather than selecting favorable cases.

## Correctness and idle behavior

The 2,880 full-UI differential cases compare every styled buffer cell and global
TableState field, including tiny/empty views, stale or maximum offsets/selection,
column selection, no selection, Unicode/newline/long labels, all-zero sizes,
navigation and resizing. Durable production tests additionally compare full
ratatui tables with placeholder tables.

Separate ordinary PTY runs on a bounded shallow 10,000-file fixture matched all
raw ANSI/style/cursor bytes for 15 post-scan phases, including first/last/pages,
narrow/tiny/restore resizes, and pane/category/extension changes. Both exited 0.
An ordinary baseline PTY used zero process CPU ticks at 100 Hz and emitted zero
terminal bytes in 3.003 seconds of post-scan idle. Forced unchanged redraw timing
is not idle CPU usage: the application blocks waiting for events.

The local evidence bundle retains raw numeric samples, full PTY transcripts,
source and binary hashes, and allocator/differential logs outside the repository.
No generated fixture, executable, target directory, reference-source copy, or
terminal transcript belongs in a published patch.
