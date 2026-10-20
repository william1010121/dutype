# Snapshot-consistent category pagination

Baseline: `682a5116cf7eb945d49a5788375f01f6bd77fc6a` (the documentation-only
successor of `3e49bc860519e899ec8907f36dd98546f9c5fe5c`; their production sources,
Cargo manifest and lockfile are identical). Candidate is the accompanying
pagination patch. No scan/classification/accounting hot path was changed.

## Contract and bounds

The opt-in MCP `find_by_category` page selector uses one completed retained
scan, not atomic filesystem truth. Legacy calls keep their existing comparator,
fields and defaults. `items_truncated` still compares the snapshot match count
to this page length; `has_more` and `next_cursor` indicate continuation.

The page comparator adds a visit ordinal only after exact selected-size/path
collisions. First pages walk once; continuations resolve their boundary in one
walk, then count/select in another. A bounded heap evicts a rejected candidate
before allocating its replacement, retaining at most P <= 500 owned candidate
paths. Rejected candidates never construct owned paths. Output entries are
constructed only for the selected page, with no offset prefix or all-match
index. Deep traversal uses the existing stack-growing walk. Workspace is
O(depth + path length + P * path length), in addition to the existing scan.

The session retains one current generation per root/filesystem-policy key.
Cursors do not retain trees, previous pages or registry entries. Continuations
perform no canonicalization, cache loading, filesystem sharing probe, device
lookup, scan or clone/private I/O. Rendering uses the generation's frozen
sharing capability and retained accounting/private metadata. Successful refresh
or capability replacement invalidates cursors; failed refresh preserves them.

Process-local randomized 128-bit epochs distinguish restarts even when the
same disk snapshot and timestamp are loaded. Epoch uniqueness is probabilistic,
like a random UUID, not a timestamp guarantee or persisted identity. A checked
atomic generation counter fails closed rather than wrapping. Fixed-width,
canonical lower-case cursor encoding and process-local fingerprints reject
changed fields; fingerprints are not cryptographic authentication credentials
or a security boundary. Ordinals and query binding are independently validated.

## Reproducible work counters

`cargo test category_page_work_counters -- --nocapture` checks and emits the
following counters. These use flat synthetic nodes, so visited nodes equal
matches; the timing fixtures below also include intermediate directories.

| Matches | Page | Offset | Walks | Node visits | Candidate paths built | Boundary paths built | Peak candidates | Output entries |
|:--|:--|--:|--:|--:|--:|--:|--:|--:|
| 600 | first 50 | 0 | 1 | 600 | 50 | 0 | 50 | 50 |
| 600 | last 50 | 550 | 2 | 1,200 | 50 | 1 | 50 | 50 |
| 20,000 | first 50 | 0 | 1 | 20,000 | 50 | 0 | 50 | 50 |
| 20,000 | last 50 | 19,950 | 2 | 40,000 | 50 | 1 | 50 | 50 |

A separate reverse-rank walk test forces 600 candidate constructions on the
first page and 100 on a late top-7 page at offset 500; peak live candidates
remain 500 and 7. This distinguishes bounded live storage from cumulative
allocation. Other instrumentation asserts zero filesystem entry-point calls
on continuations and invalid queries, stable retained-map size across pages,
and one/two tree walks. Tests cover 600-item 500+100 and 137-sized partitions,
replay, changing page size, exact rendered-path collisions, private values,
`du`/`dedup` ordering, root exclusion/pruning, deep paths and checked arithmetic.

## Before/after measurements

Linux x86-64, Rust stable 1.99.1, release/LTO, Hyperfine 2.0.0, four Rayon
threads. Fixtures contain 600 and 20,000 empty PDFs in groups of 200 per
directory. Both binaries use the same CLI-created disk snapshots. Preparation,
compilation, equality checks and boundary creation are excluded from timings.
Other coordinated heavy work was paused. Load averages before/after were
`0.11 / 0.50 / 0.39` and `0.18 / 0.50 / 0.40`; the host was not exclusively
reserved. No speedup or fixed-latency guarantee is claimed.

CLI: two reversed-order rounds, five warmups and 30 measurements per binary
per case (60 total). MCP: one loaded server per binary/root/round, five warmups
and 100 measurements per workload (200 total), with binary order reversed in
the second round. MCP times include JSON request writing and reading the
complete response, but exclude Python response decoding. First-page requests
reuse the existing snapshot. Late-page boundary creation is outside measured
loops; its offset is 500 for 600 matches and 19,500 for 20,000 matches.

| Legacy workload | Before median ms | After median ms | Change | Before peak RSS KiB | After peak RSS KiB |
|:--|--:|--:|--:|--:|--:|
| CLI 600, category top 50 | 1.425 | 1.414 | -0.8% | 2,664 | 2,688 |
| CLI 600, full top 20 | 1.297 | 1.320 | +1.7% | 2,472 | 2,566 |
| CLI 20k, category top 50 | 4.192 | 4.028 | -3.9% | 4,328 | 4,374 |
| CLI 20k, category top 500 | 4.989 | 5.124 | +2.7% | 4,830 | 4,864 |
| CLI 20k, full top 20 | 4.001 | 4.065 | +1.6% | 4,328 | 4,372 |
| MCP 600, category top 50 | 0.324 | 0.304 | -6.4% | see below | see below |
| MCP 20k, category top 50 | 1.570 | 1.585 | +1.0% | see below | see below |

| New workload | 600-match median ms | 20k-match median ms |
|:--|--:|--:|
| First page, top 50 | 0.317 | 1.669 |
| Late page, top 50 | 0.435 | 2.304 |

All 2,200 successful measured trials are in
[category-pagination-results.csv](category-pagination-results.csv), with no
outlier removal. Legacy CLI full/category values and MCP category values were
exactly equal between binaries; their stable JSON fingerprints and binary
SHA-256 values are in [category-pagination-data.json](category-pagination-data.json).
These differences do not isolate cursor overhead from process/allocator/host
variation. Later pages deliberately trade a second tree walk for bounded memory.

## MCP process memory, separately measured

The Linux RSS probe starts a fresh process for each workload, three repetitions
per cell, and sends 200 repeated requests. The table gives medians. Live RSS
comes from `/proc/PID/status` after the workload; whole-process peak comes from
`wait4` at exit. Peaks include process startup, snapshot loading, serialization
and, for late pages, creation of a boundary using <=500-item pages. They are not
page-heap-only measurements. Late workloads had equal live RSS before and after
all 200 requests in all six samples (three small, three large), not per-cursor
retention growth.

| Workload | 600 live RSS KiB | 600 whole-process peak KiB | 20k live RSS KiB | 20k whole-process peak KiB |
|:--|--:|--:|--:|--:|
| Baseline legacy top 50 | 2,824 | 10,112 | 4,432 | 10,876 |
| Candidate legacy top 50 | 3,016 | 10,240 | 4,600 | 10,876 |
| Candidate first top 50 | 2,984 | 10,240 | 4,576 | 10,876 |
| Candidate late top 50 | 3,948 | 10,752 | 5,576 | 11,216 |

The timing harness also records cumulative same-process `/proc` high-water
readings, which include preceding workloads. Those readings were sometimes
nonmonotonic in this environment, so they are not used as true process peaks
or as proof of memory bounds. Raw CLI peaks and those explicitly labeled MCP
high-water readings are retained in
[category-pagination-memory.csv](category-pagination-memory.csv); fresh-process
samples are in the JSON data file. An initial harness version also prepared the
late boundary before measuring legacy workload RSS. That incomparable run was
excluded and both rounds rerun after moving preparation to the late workload.
The table above instead uses the separate fresh-process probe.

## Reproduce and validation

Build baseline and candidate with the same release toolchain. Choose a new
output directory, then run preparation outside the timing window:

```bash
python3 bench/category-pagination.py --before "$BEFORE" --after "$AFTER" \
  --output "$FIXTURE" --prepare
python3 bench/category-pagination.py --before "$BEFORE" --after "$AFTER" \
  --output "$FIXTURE" --hyperfine hyperfine
python3 bench/category-pagination-rss.py --output "$FIXTURE"
cargo test category_page_work_counters -- --nocapture
```

Stable fmt, Clippy with `--all-targets -- -D warnings`, full stable tests and
full Rust 1.88 tests pass: 139 active tests, two pre-existing ignored tests.
The 14 subprocess pagination tests exercise cache-only mode, complete
inventories at 1/4 scan threads, root removal and lossy canonical roots,
refresh/capability/restart invalidation, failed-refresh retention, separate
roots/filesystem policies, disk-cache isolation and Linux hard-link attribution.
Synthetic tests cover clone/private rendering; this Linux run does not replace
macOS APFS, reflink-capable filesystem or Windows CI validation.
