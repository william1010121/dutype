# Scan-only cancellation of clone/shared-extent accounting

**Scope and limitation:** loop cancellation now stops disposable scan accounting, but an already-running standard-library sort remains non-interruptible. Late requests in the million-file extent fixture still wait 0.97–1.57 seconds after a 100/500 ms request. This is not a bounded-latency or generally instant-cancellation change; cancellable sorting is separate work.

Baseline: `17b23c53680a77891c57bf83c8e1482de2df358f` (PR #17). Linux x86_64, release/LTO, Rust 1.99.1, five sequential trials per synthetic case. Both baseline and candidate were built with the same toolchain and target directory. Synthetic fixtures contain 100,000 or 1,000,000 equal-size files with deterministic pseudorandom names, paired clone families (known refcount 2), or paired files sharing two contiguous 2 KiB extents. No filesystem probes run in these cases (`base=None`). Construction and accounting fingerprints are outside the operation timer. Tree drop is measured separately. Function-local allocation destruction is included in operation/request-to-return time.

`account(root, base)` remains the non-cancellable retained-tree entry point used by cache reconstruction and app subtree replacement. Only scan-time callers use `account_cancellable`, whose must-use boolean reports completion. A false result is disposable: collection may already have reset old aggregates and apply may have written only part of the result. The cancelled scan worker is joined and discarded; incomplete results must not be displayed, cached or merged. Allocated totals, representative choice, first-link `nlink` snapshot semantics and freeable rules are unchanged.

## Cancellation measurement

The helper thread sleeps for the requested delay immediately before accounting starts, sets the cancellation flag, and reports its actual request timestamp. Reported cancellation latency is actual request-to-function-return, not keyboard-to-UI exit. At 1M files, a 10 ms request often interrupts collection before any large sort. At 100k extents it can arrive during member ranking instead. Later fixed delays intentionally expose that limitation; wall-clock requests do not identify an exact internal phase and scheduling varies.

| Files | Kind | Baseline request→return ms | Candidate request→return ms (range) | Candidate tree drop ms |
|---:|---|---:|---:|---:|
| 100,000 | Clone | 13.424 | 0.005 (0.003–0.103) | 2.175 |
| 100,000 | Extent | 88.424 | 58.080 (53.006–61.036) | 5.113 |
| 1,000,000 | Clone | 361.473 | 5.104 (0.006–9.538) | 16.433 |
| 1,000,000 | Extent | 2065.864 | 3.724 (0.007–6.266) | 62.170 |

Later requests, million-file extents only:

| Requested delay ms | Request→return median ms (range) | Tree drop median ms |
|---:|---:|---:|
| 100 | 1516.692 (1401.703–1573.105) | 54.280 |
| 500 | 1086.945 (965.102–1199.009) | 51.962 |
| 1800 | 8.981 (4.507–70.000) | 58.756 |

## Uncancelled cost and equality

Median operation milliseconds. Baseline columns are two separately timed labels for the same old API, providing a useful indication of run-order noise.

| Files | Kind | Baseline ordinary | Candidate ordinary | Baseline scan label | Candidate scan | Complete fingerprint |
|---:|---|---:|---:|---:|---:|---|
| 100,000 | Clone | 25.175 | 18.340 | 22.292 | 25.351 | `f21c1dc6c005ed19` |
| 100,000 | Extent | 91.334 | 92.837 | 91.474 | 83.344 | `8b6ff4fcd1211604` |
| 1,000,000 | Clone | 414.785 | 265.491 | 384.142 | 386.801 | `71e96410dfb979e9` |
| 1,000,000 | Extent | 2283.502 | 2069.339 | 2103.285 | 2327.135 | `a8ff9cbb5337b545` |

The million-file extent scan sample is 10.6% slower than the baseline scan-labelled sample, but 1.9% slower than the separately timed baseline ordinary sample; those baseline labels call identical code. This run-order/host variance limits conclusions about small overhead. The 100k-clone scan sample is 0.7% above baseline ordinary, after avoiding redundant atomic loads at every tiny family. No broad speedup claim is made.

## End-to-end warm filesystem scan

160,000 entries per fixture; hard-link fixture has 20,000 unique 4 KiB files with eight links each across 80 directories. Hyperfine mean ± standard deviation, milliseconds; eight Rayon threads.

| Fixture | Mode | Baseline | Candidate |
|---|---|---:|---:|
| plain | ordinary | 50.32 ± 8.84 | 46.28 ± 2.79 |
| plain | clones | 47.20 ± 4.51 | 46.86 ± 2.33 |
| links | ordinary | 48.98 ± 3.88 | 47.59 ± 2.70 |
| links | clones | 71.58 ± 4.54 | 72.58 ± 4.62 |

Peak RSS from the same hyperfine processes (mean of per-run process peaks, MiB), baseline → candidate: plain ordinary 15.8 → 15.8; plain `--clones` 15.9 → 15.9; hard links ordinary 26.2 → 26.4; hard links `--clones` 33.9 → 33.8. These are full-process measurements, not just accounting scratch, and show no material memory regression on these fixtures. The synthetic accounting harness does not claim a separate peak-RSS measurement.

## Correctness and tests

Final validation on the exact production/test code: `cargo fmt --check`, stable `cargo clippy --all-targets -- -D warnings`, and full `cargo test` on Rust 1.99.1 and MSRV 1.88.0 passed. Each test run passed 75 unit tests plus 14 integration tests; two manual benchmarks were ignored in ordinary test runs; the new synthetic accounting benchmark was run explicitly as described above. MSRV clippy was not run because that optional component is not installed; stable strict clippy passed.

Complete results are fingerprinted outside timing: every node's allocated/deduplicated/freeable values and category totals; each child's dedup/freeable result; every shared record's identity, link count, refcount, duplicate flag and extent credit. Baseline, candidate ordinary accounting and candidate scan accounting match for every completed synthetic run at both sizes. These fingerprints supplement the existing invariant tests, rather than replacing them.

Deterministic callback tests cancel at every reachable boundary on fresh and previously accounted synthetic trees, including a single large family, many tiny families, one file with hundreds of extents, nested collection and apply. Cancelled partial trees retain allocated/category/index invariants and are discarded, not reused. A fake family-size provider and a one-thread Rayon pool exercise all `Some(base)` planning/probe/application boundaries and assert that no further probe starts after cancellation is observed. Explicit tests cover varying `nlink` snapshots and identical extent offsets on different devices. Completed cancellable accounting matches ordinary accounting and is idempotent.

Polls cover collection, per-file extent collection, group construction, group member reductions, probe planning and application, extent preparation/event construction/sweep, directory propagation and result application. Absolute member positions avoid repeatedly polling at the start of tiny groups. Retained accounting uses a constant-false callback that optimizes away checks. No entry-by-entry scan syscalls, new scan locks, sort comparator interruption or sort/remapping redesign were added.

The pre-existing Linux FIEMAP preparation/probe/application pass in `read_shared_extents` is outside this accounting-only change. Its existing cancellation behavior is not strengthened here.

Uninterruptible work still includes each individual sort, an already-running clone-refcount syscall/path resolution, allocator growth/zeroing, scratch destruction, and retained-tree destruction on the worker's exit. Parallel probes are awaited before returning. Neither these synthetic timings nor Linux-only local tests claim real APFS syscall, macOS/Windows runtime or end-to-end UI latency coverage.

## Reproduction

In the candidate checkout (normal Cargo/Rust toolchain environment):

```sh
cargo test --release synthetic_account_cancellation -- --ignored --nocapture
for delay in 100 500 1800; do
  DUTYPE_LATER_CANCEL_ONLY=1 DUTYPE_CANCEL_DELAY_MS=$delay \
    cargo test --release synthetic_account_cancellation -- --ignored --nocapture
done
```

For the baseline, create a disposable detached worktree at the exact baseline SHA, copy `bench/account-cancellation.rs` into its `bench/` directory and append this test-only registration to `src/main.rs`:

```rust
#[cfg(test)]
#[path = "../bench/account-cancellation.rs"]
mod account_cancellation_bench;
```

Replace only the benchmark's `cancellable` wrapper with `let _ = flag; account::account(root, None); true`, since the baseline has no cancellable accounting API. This deliberately lets the helper set the flag while the old function continues to completion. Run the same command, using the same release profile and toolchain. The baseline's ordinary/scan labels both call its original `account`; their differing timings illustrate host/run-order variance.

End-to-end hyperfine commands use the existing synthetic filesystem fixtures generated by `bench/hardlinks.py`, an empty configuration, eight Rayon threads, two warmups and ten runs per binary:

```sh
hyperfine --warmup 2 --runs 10 --env RAYON_NUM_THREADS=8 \
  'BASELINE --json [--clones] --config EMPTY_CONFIG FIXTURE' \
  'CANDIDATE --json [--clones] --config EMPTY_CONFIG FIXTURE'
```

Run both `plain` and `links` fixtures, each with and without `--clones`. Raw logs and JSON are retained as local validation artifacts rather than committed. Timings are from a shared host with a coordinated quiet window, not a dedicated benchmark machine; do not interpret small differences as stable speedups or regressions.
