# Filesystem and output edge-case regressions

Baseline: `ee2af7e43b96cb8cd6c19b04a68c91752a0b88ed`.
Validated on Linux x86-64, unprivileged uid 1000, Rust 1.99.1; Rust 1.88.0
(the declared MSRV) also passes `cargo check --all-targets --locked`.
All filesystem data in these tests is synthetic and temporary.

## Coverage matrix

| Area | Evidence and outcome |
| --- | --- |
| Empty directories | Complete JSON schema, zero files, finite percentages, additive category totals, repeat stability. Already correct on baseline. |
| Deep directories | Baseline debug CLI aborts with stack overflow at 1,500 nested short names. Now CLI JSON, cache writing, repeated MCP scans/cache replacement, and destruction pass. A 350-level hardlink tree also scans, accounts, reports and drops on a 128 KiB thread stack. |
| Symlink loops | Child self/ancestor cycles are counted as entries, not followed. A cyclic root exits with an error without hanging or emitting invalid JSON. Already correct on baseline. |
| Permission failures | Unreadable roots and descendants produce partial JSON with an explicit unreadable count; readable siblings and category totals survive. Tests verify that permission denial actually occurs and explicitly skip that assertion for privileged environments. Ran without skipping here. Already correct on baseline. |
| Disappearing files | Unix deterministic hook removes an enumerated entry immediately before metadata lookup: exactly one error, healthy sibling retained. Bounded concurrent churn checks valid output and a stable exact rescan after mutation stops; it is not used as proof that a race occurred. Existing handling was correct. |
| JSON/MCP stability | Repeated schemas/order, absolute paths from relative roots, Unicode/newline filenames, matching text/structuredContent, closed stdout, valid-notification silence, malformed-message recovery, and correct protocol/execution error separation. Fixed TopN tie selection and malformed MCP handling. |

New integration tests are discovered by the existing Linux/macOS/Windows
`cargo test` CI matrix. Each CLI/MCP subprocess has a 20- or 30-second deadline
and is killed/reaped on timeout; the CI job itself is capped at 20 minutes.
Unix-specific symlink, permissions and deterministic metadata-race tests are
gated. Deep fixtures use 1,500 levels on Linux and 400 on other Unix platforms
because macOS has a smaller path-length limit. Windows/macOS were not run locally.

## Regression-first evidence

- `deep_directory_json_finishes_without_stack_overflow`: before, child aborted
  with `thread 'main' has overflowed its stack`; after, completes with valid JSON,
  zero files and zero unreadable entries. Also exercises cold/repeated CLI cache.
- `top_n_ties_use_the_same_path_order_as_sorting`: before, largest two zero-byte
  files were `b.rs, a.rs`; after, `c.rs, b.rs`, matching the existing descending
  `(size, Path)` comparator. Both report and category-find are asserted.
- Three MCP unit regressions failed before: invalid envelopes, invalid tool-call
  parameters, and options violating the advertised schema. A stdio test also
  failed because a `null` message disappeared without an Invalid Request response.
  They pass after validation; a following valid request still gets its response.
- Eight filesystem integration tests and the closed-stdout test were also run
  against an isolated archive of the original revision and pass there. These
  are additional coverage, not claimed prior defects.
- Original full suite: 56 passed, one failed because the FIEMAP cancellation test
  unconditionally required FIEMAP support from the temporary filesystem. The
  cancellation assertion still always runs; only `ENOTTY`/`EOPNOTSUPP` from the
  additional non-cancelled probe are accepted as unsupported. Other errors fail.

## Implementation notes

Recursive scan/report/account/cache-write paths use conditional stack growth;
normal recursion only checks remaining stack, growing a segment near exhaustion.
`DirInfo` destruction drains descendants iteratively, including on background
MCP threads. No per-entry syscall or lock was added to the directory scan.
The production metadata-race hook is an empty monomorphized closure.

TopN filtering now respects the same path tie-breaker as its heap comparator.
Equal-size sibling runs are visited in reverse name order during report traversal,
so tied files do not each force a new allocation/heap replacement. Public child
ordering and the existing `Path` comparator semantics are unchanged.

MCP rejects invalid envelopes with `-32600`, invalid tool names/arguments with
`-32602`, and keeps actual execution failures as `isError` tool results. Valid
notifications remain silent. Successful public output fields are not removed
or renamed. Clients sending out-of-schema values (e.g. `top: 0`) must now fix
those values instead of relying on silent clamping/defaults.

## Checks

Run:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo test --release --locked
cargo +1.88.0 check --all-targets --locked
```

The standard CI smoke commands also apply:

```sh
cargo run --release -- --report --top 5 .
cargo run --release -- --json --category Code .
```

## Performance

Release binaries, hyperfine 2.0, warm filesystem cache, 3 warmups, 20 measured
runs. Fixtures contain equal-sized 256-byte `.rs` files, intentionally stressing
TopN tie comparisons. Results are not a general filesystem benchmark.

| Fixture | Original | Changed | Difference |
| --- | ---: | ---: | ---: |
| 20,000 files / 200 dirs | 6.7 ms | 7.7 ms | +14.4% |
| 200,000 files / 500 dirs | 48.4 ms | 55.1 ms | +13.8% |
| 200,000 files, reversed benchmark order | 44.7 ms | 49.4 ms | +10.5% |

The 20,000-file `--top 0` comparison (no TopN path comparisons) was 6.5 ms
original versus 6.4 ms changed, within noise. Thus the measured worst-case
report overhead is primarily the newly correct tied-path comparisons, not
stack growth. Memory was essentially unchanged (~17.4 MiB on the large fixture).
An initial naive tie fix cost 2.2x on the smaller fixture; reversed tied-sibling
traversal eliminated that allocation-heavy regression. No ordering semantics
were relaxed to obtain the final numbers.

## Explicit limits

- These are live scans, not filesystem snapshots. A file removed after successful
  metadata collection can still be represented; concurrent readdir can repeat
  a name after remove/recreate. No exact in-flight file-count upper bound is claimed.
- Static symlink loops are covered. A directory replaced by a symlink between
  enumeration and descent, especially via a replaced ancestor, is not confined
  by descriptor-relative traversal. Fully closing that TOCTOU race is separate work;
  an extra path precheck would not solve it.
- Hardlinked bytes are counted once, but the parallel first winner can vary the
  path/category attribution. Repeated-JSON equality fixtures use independent files.
- The existing cache reader rejects trees deeper than 256 as a cache miss; scans
  and cache writes remain safe, but very deep persisted snapshots are rescanned.
- Interactive TUI-only recursive operations are not claimed stack-safe by this
  report/MCP-focused change. Real APFS/reflink clone support was not available on
  this Linux temporary filesystem; existing synthetic accounting tests still run.
