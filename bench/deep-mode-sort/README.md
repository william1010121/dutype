# Bounded size-mode sorting microbenchmark

This is a sort-only comparison of the original recursive `app::sort_tree` at
`4baa74f2f15c51507e128056fd4447ed5d7d4523` and its iterative replacement at
`1476a87f75fad5ed4a7b46ddbd6836b3eee5ad4f`. It is not a whole-TUI benchmark.
The summary and limitations are in [the deep-tree testing guide](../../docs/deep-tui-testing.md).

## Reproduce

With Rust 1.99.1, Python 3 and a C compiler already available:

```sh
python3 bench/deep-mode-sort/build.py --output /tmp/dutype-sort-bench
cc -O2 bench/deep-mode-sort/rss_exec.c -o /tmp/dutype-sort-bench/rss_exec
# Wait until other builds/benchmarks are idle before measuring.
python3 bench/deep-mode-sort/measure.py --output /tmp/dutype-sort-bench
```

The output path must be new. The build uses local git objects at the pinned
revisions; it never fetches or installs anything. It respects existing
CARGO_HOME/RUSTUP_HOME/PATH and does not change toolchain defaults.
The measurement runner is Linux-only, sequential, and has a 60-second execution
budget, not a performance pass/fail threshold. It refuses to replace an existing
results directory. It does not change system settings or CPU affinity.

## Source fidelity and boundaries

The builder extracts actual Node, DirInfo, CloneInfo, Shared, Reflink and their
accounting/sorting methods, plus the actual Category declaration/derives/index
method. Before and after must have identical scan/category source. Only the
registry is a one-category stub, used outside the timed sort. Source/binary hashes,
compiler details and type sizes are recorded. Release flags are opt-level=3,
lto=true and codegen-units=1.

Normal binaries measure latency and whole-child-process peak RSS. A separate
safe instrumented binary records the iterator vector's maximum capacity in
bytes. There is no allocator recorder in timing. Fixtures, digesting, output
and teardown are outside timing. Each sample averages a case-specific batch of
dedup+du toggle pairs; there are three warmups and fifteen samples. Candidate
order alternates by case. Every before/after full-state digest must match.

RSS uses wait4/rusage for three fresh child processes per candidate/case and
includes startup, the tree and teardown. It does not isolate traversal allocation
or prove an allocator byte-count bound. Iterator capacity excludes the unchanged
width-sized scratch arrays inside `DirInfo::sort_children`. The non-clone root
returns before either sorting or traversal allocation.

## Recorded evidence

[recorded/raw.ndjson](recorded/raw.ndjson) preserves every corrected-run sample,
command, digest, RSS result and capacity observation. [summary.json](recorded/summary.json)
contains the derived medians/ranges. [source-manifest.json](recorded/source-manifest.json)
and [provenance.json](recorded/provenance.json) identify the source and binary
hashes, revisions and measurement method. Historical absolute command paths are
provenance, not paths to use for reproduction.

The corrected run took 1.46 seconds on x86_64 Linux with Rust 1.99.1 after other
known build/test workers became idle. That coordination and local process checks
cannot establish globally exclusive CPU access. These finite synthetic samples
are not a broad performance guarantee.

An earlier prototype used a zero-sized Category stub rather than the production
Category(u8). Its measurements are excluded. Only the corrected source-faithful
run is recorded here; portable scripts were adapted afterward without changing
the measured hot-path code or reconstructing any samples.
