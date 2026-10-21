# Opt-in mixed-category inventory: validation

## Scope and compatibility

Baseline: main commit `2069742dea998fd99bdd7f708fb9dace870124d7`.

`--include-files` with `--category` and `--report`/`--json`, or MCP
`find_by_category` with `include_files: true`, adds directly matching files to
mixed directory/file categories. Both scanned category and direct filename
classification must match. Final extension ownership and Other's fallback
apply. Matching directories prune descendants even below the top-N cutoff;
the queried root remains excluded. Pure-category matches remain unchanged.
The effective boolean is bound to pagination cursors and echoed only when true.
Defaults, existing fields, accounting and TUI behavior are unchanged.

The registry helper runs once per opted-in directory-category query. It adds
no scan-time classification work, default-query map walk, extra tree traversal,
or candidate-path allocation. Report and pagination use the same predicate.
Top-N/path allocation bounds, ranking and directory-pruning behavior remain.

## Correctness checks

- Rust 1.99.1: formatting, all-target Clippy with warnings denied, full tests,
  and release build passed.
- Rust 1.88.0 (MSRV): full tests passed. Its Clippy component was unavailable;
  Clippy validation used 1.99.1 only.
- Both test suites: 128 unit tests and 45 integration tests passed; two existing
  tests remain ignored.
- Independent source review and focused CLI/MCP probes found no issues.

Coverage includes the README Unity example; final-rule overrides; Other
fallback; excluded roots; inherited and weak-directory category mismatches;
nonempty pure-file and pure-directory categories; zero-size files; hardlinks;
CLI top=0; pruning when a directory is not retained; tie ordering; synthetic
private/clone/freeable accounting; 606 mixed matches paginated with 1/4 scan
threads and both size modes; replay and changed page sizes; option/cursor
mismatches; and invalid CLI/MCP combinations. Independent probes also exercised
omitted/false continuation equivalence and continuation after the root vanished.

### Exact legacy output comparisons

All 70 baseline/candidate comparisons passed byte-for-byte, without
normalization:

- 48 fresh category CLI outputs: four category kinds, JSON/text, top 0/1/100,
  and du/dedup modes.
- 20 cached CLI outputs: full and category JSON/text in both size modes,
  reading the identical saved scan and its metadata.
- Two complete MCP output streams containing ten full/category default tool
  replies, reading the same saved scans.

Comparisons use the same absolute fixture paths. Cached full reports avoid
comparing independent nondeterministic scan times. Help and tool schemas
intentionally change to document the opt-in; they are not part of the legacy
payload-equivalence claim.

## Bounded performance measurements

Hyperfine 2.0.0, Linux x86_64, one Rayon scan thread, Rust 1.99.1 release builds
with identical flags. The repository's `bench/run.sh` exists, but its
machine-specific `~/bench/tools.sh` dependency was unavailable; measurements
used Hyperfine directly.

Fixture: 12,000 loose `.unitypackage` files, 600 matching `Library` directories,
and 1,200 files pruned inside those directories. Top=50. Default queries match
600 items; opt-in queries match 12,600. Baseline and candidate use the same
configuration and saved scan.

The CLI controls used these commands with the fixture/config paths substituted
and `RAYON_NUM_THREADS=1`, `DUTYPE_CACHE_DIR` pointing at the fixture's cache:

```sh
hyperfine --shell=none --warmup 5 --runs 20 \
  "$BASELINE --report --config $CONFIG $ROOT" \
  "$CANDIDATE --report --config $CONFIG $ROOT"
hyperfine --shell=none --warmup 5 --runs 25 \
  "$BASELINE --json --cache --category Unity --top 50 --config $CONFIG $ROOT" \
  "$CANDIDATE --json --cache --category Unity --top 50 --config $CONFIG $ROOT"
```

MCP batches started `--mcp --cache --config $CONFIG`, primed the retained scan,
then made 150 `find_by_category` calls with `path: ROOT`, `category: "Unity"`,
`top: 50`. Page batches first requested `paginate: true`, then repeated its
returned cursor. Opt-in batches also sent `include_files: true` on every call.
Hyperfine used `--shell=none --warmup 3 --runs 12` for each sequential batch;
each interleaved pair used `--shell=none --runs 1` without warmup.

All timings are whole-process measurements, **not predicate-only timings**.
Cached CLI includes startup, config parsing and cache deserialization. MCP
batches include Python/server startup, initial cache loading, 150 retained
requests, JSON serialization/parsing and IPC. Continuation batches repeatedly
request the same second page, using two tree walks per request.

### Initial sequential measurements

Median milliseconds:

| Identical default workload | Baseline | Candidate | Runs / warmups |
| --- | ---: | ---: | ---: |
| Full scan and text report, no cache | 36.844 | 38.000 | 20 / 5 |
| Cached category CLI | 3.088 | 3.528 | 25 / 5 |
| 150 retained category requests | 195.490 | 181.580 | 12 / 3 |
| 150 continuation requests | 220.220 | 286.914 | 12 / 3 |

The apparent continuation slowdown prompted an interleaved repeat rather than
an optimization based on one sequential sample. No implementation edits were
made between these measurements.

### Interleaved continuation repeat

Twelve Hyperfine pairs alternated baseline/candidate order (AB/BA), with 150
requests per process and 600 matches per request:

| Statistic, milliseconds | Baseline | Candidate |
| --- | ---: | ---: |
| Median | 238.042 | 251.175 |
| Mean | 239.474 | 252.291 |
| Standard deviation | 10.486 | 16.073 |
| Minimum | 226.336 | 231.432 |
| Maximum | 264.045 | 280.727 |

The median within-pair candidate/baseline ratio was **1.0242**. Individual
ratios ranged from 0.9747 to 1.2216. The initial roughly 30% sequential gap did
not reproduce consistently. These noisy container measurements show bounded
small-overhead observations, **not a speedup or proof of zero regression**.
A host load-average sample was 0.20/0.57/0.60 on a 9-vCPU environment.

### Opt-in measurements, different work

Separate opt-in medians were 236.764 ms for 150 retained query requests and
368.177 ms for 150 continuation requests (12 runs after 3 warmups each). They
match 12,600 rather than 600 candidates; top remains 50. They are not an
equivalent-work regression or speedup comparison with the default results.

All benchmark children exited successfully. Raw sequential and paired timing
samples, scripts, source/binary hashes, legacy comparisons and build/test logs
are preserved in the local validation record.

## Limits

Local validation did not include live APFS private/reflink or Windows execution. Synthetic
accounting tests verify preserved output values, not native behavior on those
filesystems. This is a bounded category-inventory change and measurement, not a
new scan-performance optimization or general filesystem audit.
