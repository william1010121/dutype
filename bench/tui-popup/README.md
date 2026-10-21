# Bounded popup top-500 benchmark

This portable harness builds real production-source copies and keeps compilation separate from explicitly authorized measurement. Only the fresh authorized run in recorded/ is retained. No missing historical results or raw data were reconstructed. The repository report bench/tui-tie-parity.md owns the narrative analysis; recorded/summary.json contains all eight cases.

## Build only

Requirements: Python 3.9+, Rust 1.99.1, a C compiler on Linux, and the locked dependencies already available in the Cargo cache. `build` uses offline, locked release builds with the checkout's release profile. It pins `RUSTUP_TOOLCHAIN=1.99.1` for subprocesses, respects CARGO_HOME/RUSTUP_HOME/PATH, and never changes toolchain defaults.

```
python3 compare.py build \
  --before /path/to/clean-before-checkout \
  --after /path/to/final-after-checkout \
  --before-buggy \
  --output /path/outside/both/checkouts/popup-build
```

The output must be new. Both source checkouts may contain local edits or be non-git frozen trees. Both must have the same Cargo.lock. The script copies their actual app, scan, category, report, ui, account, cache and nested modules, then replaces only the standalone main entry point and adds a metrics-only Cargo feature. Production-module content and lockfile hashes, source paths, available git provenance and toolchain versions are recorded.

Each candidate gets its own target directory. Four executables are retained: before/after × ordinary-allocator timing/instrumented allocation metrics. Identity-only smoke checks verify the label, expected correctness, metrics feature and app-source hash; ordinary and instrumented binaries are not interchangeable. No tree is constructed and no popup is opened during `build`.

Use `--before-buggy` for the fresh merged-main versus corrected-candidate comparison. It expects the original equal-size cutoff bug only in the before revision. Omit that option only when both inputs already have correct ranking. This changes untimed verification expectations, not timed controls.

## Measure only after authorization

```
python3 compare.py measure \
  --output /path/outside/both/checkouts/popup-build \
  --authorized --timeout 30
```

`--timeout` is the authorized whole-suite budget, not permission to extend a quiet window. Active child process groups are stopped at the deadline or on interruption. Use a new `--results-name NAME` for later authorized runs rather than overwriting results.

The matrix is two candidates × 10,000/100,000 PDF files × ascending ties/descending allocated sizes × category/extension popup. Each case gets:

1. One untimed correctness process, comparing actual report output and explicit expected selected paths; ordinary filesystem paths are checked too.
2. One ordinary-allocator process, constructing one tree, opening two warmups and seven measured popups.
3. One separately instrumented process counting one popup opening.
4. One fresh ordinary-allocator process for full-process peak RSS.

This is 64 harness invocations, plus RSS-wrapper/identity tooling. Candidate execution order alternates between cases, and no measurements run concurrently. Exact child commands, raw samples, medians/ranges, counters, source/binary identities, timeout and final status, and before/after host/process/hash evidence are saved. Historical absolute paths in an eventual result file are provenance, not reusable commands.

## Fixtures and correctness distinction

Every fixture is one flat in-memory Node root containing ordinary UTF-8 PDF names of equal length (`f00000000.pdf`, etc.). Category and extension use the same tree and explicit breakdown key. The actual `App::handle_key(Enter)` path runs from `Focus::Breakdown`. There is no disk scan, real filesystem fixture, rendering, external open or clipboard action. Ordinary-name path resolution constructs paths without filesystem queries.

- Ascending ties: every file has allocated size 4096. Correct later candidates outrank the current minimum, maximizing admissions and replacements. The buggy baseline incorrectly retains the first 500; the corrected candidate must retain the last 500. Tie-case timings compare different outputs and must not be called equivalent-output regression overhead.
- Descending control: sizes are `(N-index)*4096`. Both candidates retain the first 500 and reject later entries. These are equivalent-output controls.

The correct popup ranking still owns display paths and target-index vectors for admitted candidates. Deferring external filesystem paths until final survivors reduces work but does not eliminate all candidate allocations. Existing per-file extension-key allocations remain part of extension-popup cost.

## Measurement boundaries

Timing binaries compile without the allocator recorder. The timed interval covers only the production Enter handler and a small invariant/black-box wrapper. Registry/tree/App/breakdown construction, result verification, digest/JSON output and popup destruction are outside this interval. One tree is reused across the nine openings. This does not measure terminal rendering latency.

Allocation metrics enable a local Rust global-allocator recorder only during one popup open. Counts include successful zeroed allocations, with reallocations listed separately. Allocation byte traffic includes the full requested new size on successful realloc and removes the old requested size. Peak-live is net live requested Rust memory; it excludes allocator-internal realloc transients, usable-size rounding and direct libc allocations. Retained bytes are recorded while the popup exists, before drop. Tree/registry/App construction and output allocations are excluded.

A 500-item retained candidate bound is not a bound on lifetime allocation traffic. Worst-case correct ties can admit all N entries while retaining only 500 at once. Do not conflate total allocated bytes, peak-live requested bytes, retained bytes and resident memory.

Fresh-process peak RSS includes startup, registry/tree/App construction, one popup, output and teardown. The runner prefers GNU `/usr/bin/time` with `%M` if installed. Otherwise the explicitly labeled Linux `wait4`/`rusage` wrapper captures the individual child's maximum RSS in KiB. It is not incremental popup-only memory. Non-Linux systems need a compatible GNU time binary at that path or an explicitly adapted RSS provider.

Seven samples in one bounded synthetic session do not establish broad performance claims. No CPU affinity or scheduling settings are changed. Before/after host-load and process snapshots document the window, but visibility in one PID namespace cannot prove globally exclusive CPU access.

## Output files

The external build directory contains frozen source copies, separate targets, binaries, build logs, source-manifest.json, binary identities and toolchain metadata. These generated files should not be copied into a repository.

The selected results directory contains raw.ndjson, summary.json, metadata.json, run-config.json and host/process/hash evidence. A failure or timeout preserves partial results and an explicit nonzero status. No commit, push, merge or publishing is performed.
