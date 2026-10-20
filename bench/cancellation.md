# Hard-link postpass cancellation

Measured 2026-10-20 on Linux x86_64 (AMD EPYC 9V74, nine visible logical
CPUs), Rust 1.99.1 release profile. Baseline is main commit
`600d5d52ccd830efb8dcb9904b0f274ecdd0cd83` (same tree as reviewed
`4e1bff6313512ac000494916b21186fb981690b5`). No cache flushing or user data.

## Cancellation boundary

The TUI sets the cancellation flag, joins the scan worker, and discards its
result. Previously, a cancellation arriving at the traversal/postpass boundary
still ran the complete hard-link attribution pass. The new pass polls before
starting, every 256 shared records/children/inode groups, and around directory
sorting. A byte transfer's subtract/add pair is never interrupted. Cancelled
partial trees must not be published or cached. The existing TUI discard path
provides that guarantee.

The ignored test `account::synthetic::measure_cancelled_hardlink_postpass`
builds eight sibling directories with eight links per inode, with all bytes
initially in the last directory. It sets cancellation before attribution,
times the postpass, then separately times destruction. Five repetitions per
size; all times below are milliseconds (median, with range).

| Entries | Baseline postpass | New postpass | Baseline destruction | New destruction |
|---|---:|---:|---:|---:|
| 160,000 | 9.437 (7.413–12.013) | 0.001 (0–0.001) | 3.798 (3.482–4.568) | 3.898 (3.459–4.276) |
| 1,280,000 | 221.787 (155.687–250.738) | 0.001 (0.001–0.001) | 35.835 (27.204–36.638) | 31.760 (31.208–51.182) |

Reproduce the new measurement with:

```sh
cargo test --release measure_cancelled_hardlink_postpass -- --ignored --nocapture
```

This measures avoided work at a specific cancellation boundary, not end-to-end
keyboard latency. Tree destruction, an in-flight filesystem call, a single
directory sort, and a deep ancestor transfer still take time. Already-running
clone-family accounting is not made cancellable here; the scan now avoids
starting it when cancellation is already set. There is no strict wall-clock
cancellation bound.

A deterministic test injects cancellation at every polling site, both with
and without clone tracking. It checks category and ancestor totals,
count-once bytes per inode, unchanged free-base accounting, partial transfers
only at whole-inode batch boundaries, and successful uncancelled completion.
No test relies on wall-clock thresholds.

## Uncancelled scans

The ordinary entry-processing loop is unchanged. Non-hardlink scans perform
one additional relaxed atomic load after traversal. Final attribution also
avoids recursive calls on hardlinked file leaves, which have no children.

The existing `bench/hardlinks.py` fixture contains 160,000 entries in 80
sibling directories: either independent 4KiB files or 20,000 inodes with
eight links each. Both binaries use `--json`, an empty explicit config, no
cache, and `RAYON_NUM_THREADS=1/2/4/8`. Twenty randomized/interleaved warm
repetitions per case were recorded during a coordinated quiet window.
The first warmup is excluded. Whole-process wall time and peak RSS are
measured by Python monotonic time and `wait4`; application scan time is
rounded. Host one-minute load decreased from 2.40 to 1.78.

| Tree | Threads | Baseline wall ms | New wall ms | Change | Baseline/new peak RSS KiB |
|---|---:|---:|---:|---:|---:|
| plain | 1 | 187.03 | 188.14 | +0.6% | 14176/14180 |
| plain | 2 | 104.36 | 103.08 | -1.2% | 14430/14404 |
| plain | 4 | 63.61 | 65.99 | +3.7% | 15068/15118 |
| plain | 8 | 43.74 | 44.57 | +1.9% | 16130/16090 |
| links | 1 | 192.40 | 189.96 | -1.3% | 24898/24930 |
| links | 2 | 107.08 | 103.47 | -3.4% | 25204/25260 |
| links | 4 | 65.25 | 64.13 | -1.7% | 25826/25792 |
| links | 8 | 48.03 | 49.17 | +2.4% | 27012/27028 |

These small differences change direction with thread count; they do not
establish a throughput improvement or a meaningful regression. An earlier
12-repeat interleaved batch without a coordinated quiet window ranged from
−5.6% to +12.0% on ordinary trees and −6.3% to +8.3% on hardlink trees.
Do not infer speedup from the cancellation-boundary measurement.
Full JSON output, excluding elapsed time, matched baseline for both trees
at all four thread counts. Peak memory remained essentially unchanged.

A separate 20-run hyperfine check at eight threads measured ordinary means
50.5 ± 7.2ms before and 47.0 ± 3.4ms after; hardlink means 52.8 ± 5.6ms
before and 53.7 ± 9.2ms after. This also supports treating small throughput
changes as noise, not a claimed optimization. An initial hyperfine attempt
overlapped correctness scans and was excluded; the reported run was repeated
without concurrent local validation. Raw timing files are retained with the
validation artifacts.

Binary SHA-256:
- baseline: `a32a7227f68d280b3872922986ee4e9acd27363a73c9264bea7239097e1ec0f9`
- cancellable: `20bebedef8c9b899e32cf8c8f1b9f3490d0a06d0e32ad04b670a2b689e0ba08d`
