# TUI top-500 tie parity

## Scope

This candidate is reconstructed on merged PR #25, base `348ac88a26936c6bd2e98dabf9b0ff68184bc983`. The previous uncommitted candidate and benchmark artifacts were lost with an execution-environment replacement. This is a newly reviewed and validated implementation, not a byte-identical recovery; only fresh evidence is used below.

The historical bug is independent of result jumping: with 501 equal-sized PDFs, CLI/MCP select `f0500.pdf` through `f0001.pdf`, while the old TUI selected `f0499.pdf` through `f0000.pdf`. The fresh regression failed before the fix and passed afterward.

Production changes are confined to `src/app.rs`:

- Admission, heap ordering and final display share one comparator: descending effective size, then rendered `Path` order. Path-component ordering, not a slash-string approximation, matches report ranking.
- Exact size/display-path collisions prefer the earlier captured snapshot index chain (DFS order). Filesystem paths are payload only. Legacy report ordering among indistinguishable display keys is unspecified, so identity-order parity with that legacy report is not claimed.
- Rejected candidates build no owned display path or index chain. Traversal state grows with depth; only 500 candidates are retained. Worst-case ascending ties can still admit and evict every candidate.
- Only final survivors resolve existing external-action paths, using their captured indices and the existing `resolve_child` semantics, before the popup is published. Missing indices or unresolved raw names yield `None`; ordinary nonexistent paths remain constructible as before. Jumping still uses snapshot identity, never a display-name lookup. Resolution races merely shift within the existing synchronous, non-atomic popup-open operation.

No scan hot-path, CLI/MCP schema, jump implementation, or extension-breakdown tie ordering changes are included.

## Fresh validation

On Linux, Rust 1.99.1 `fmt --check`, `clippy --all-targets -- -D warnings`, and `test --locked` pass. Rust 1.88.0 `test --locked` also passes: **158 tests**, with two existing manual benchmarks ignored.

Nine new unit-test groups cover the explicit reproduction; exact 499/500/501 boundaries; zero, mixed and deduplicated sizes; category/extension parity; nested Path ordering; special-directory pruning; exact display collisions and earlier-identity cutoff; filesystem-path-independent ranking; and retained raw/literal/unavailable path semantics. The original nine PR #25 jump tests remain unchanged. Unix raw-name resolver unit coverage is synthetic so it does not require macOS filesystems to accept invalid UTF-8 entry names.

Fresh real-PTY runs:

- `tests/tui_tie_parity.py`: 14 scenarios, 6,996 exact ordered-path comparisons against CLI JSON, and 14 last-row jumps. PageDown waits for the intended selection to render instead of relying on fixed short sleeps. Zero-only categories remain intentionally hidden, so zero-only PTY cases use extension mode.
- Unchanged `tests/tui_result_jump.py`: five scenarios and 13 jumps, including repeat/back, resize, cancellation, refresh, directory/empty cases and raw names.
- Independent review: 18 focused tests plus six additional PTY scenarios, 2,000 ordered cutoff rows and eight jumps, including equal-size actual raw/literal display twins on Linux.

All final PTY sessions restored terminal state; every checked jump produced zero fixture `IN_OPEN`/`IN_ACCESS` events. These counters do not trace metadata-only syscalls. PTY sessions isolate `PATH` from external commands and do not send reveal/open/copy keys.

During reconstruction, an intermediate unit helper mistakenly reused Enter while a prior popup was open, which could invoke the reveal handler on synthetic or temporary fixture paths. That handler may have spawned `xdg-open`; its suppressed output means the outcome cannot be established. No leftover external-opening processes were observed. The final new collector helper sets state and calls `open_popup()` directly, eliminating external-action dispatch; all final tests were rerun. This disclosure is separate from the controlled final PTY results, not a claim that no intermediate external action was ever attempted.

Windows/macOS runtime validation remains pending CI.

## Fresh bounded performance comparison

The portable harness and raw evidence are in [`tui-popup/`](tui-popup/). It compares merged-main's buggy baseline with this corrected, deferred-resolution implementation. No lost historical measurements are reconstructed or reused.

One coordinated Linux quiet-window run on 2026-10-21 completed **64/64 processes in 2.49 seconds**, with a 30-second whole-suite limit and all executable hashes unchanged. Each of eight fixtures has separate before/after verification, uninstrumented timing (two warmups, seven samples), instrumented one-open allocation metrics, and fresh-process peak RSS. Fixture/App construction, rendering, output and popup destruction are excluded from popup timings. Full RSS includes process startup, fixture construction and teardown.

Median popup-opening time in milliseconds:

| Files | Pattern | Popup | Buggy baseline | Corrected candidate |
|---:|---|---|---:|---:|
| 10,000 | ties | category | 0.350 | 5.880 |
| 10,000 | ties | extension | 0.525 | 6.073 |
| 10,000 | descending | category | 0.337 | 0.312 |
| 10,000 | descending | extension | 0.597 | 0.457 |
| 100,000 | ties | category | 2.905 | 64.424 |
| 100,000 | ties | extension | 4.273 | 60.393 |
| 100,000 | descending | category | 2.713 | 2.663 |
| 100,000 | descending | extension | 4.308 | 4.736 |

**Ascending-tie timings compare different outputs:** the baseline incorrectly retains the first 500 paths; the corrected candidate retains the last 500. Real scan children with equal sizes are name-ascending, so this fixture deliberately maximizes correct admissions/evictions. The extra time and allocation traffic are a correctness cost, not equivalent-output overhead. Descending-size controls return identical subsets. The 100k extension control was 4.31 to 4.74 ms in this run; results do not support a uniform speedup claim.

Allocation and memory findings:

- At 100k ties, category allocation calls were **2,006 to 201,006**, and requested allocation bytes **134,591 to 2,224,091**. Extension calls were **102,006 to 301,006**, and bytes **434,583 to 2,524,083**. Existing extension-key allocations remain included.
- Reallocation counts were unchanged: category 501 and extension 500. Ordinary descending controls had identical allocation traffic before/after.
- Popup-only peak live requested bytes fell from **112,538 to 90,538** in every case. Retained popup bytes stayed **72,516**, independent of the 10k/100k fixture size. Lifetime allocation traffic is not retained memory.
- Fresh-process peak RSS at 100k was approximately **8.4 MiB** in both variants. Small differences in one process per case are not evidence of a resident-memory improvement. The Linux `wait4`/`rusage` provider is explicitly labeled; GNU time was unavailable.

Allocator figures exclude allocator-internal overhead/reallocation transients and direct libc allocations. Seven timing samples in one bounded synthetic run do not establish general application performance. No affinity or scheduling settings were changed. Host load/process snapshots are retained, but visibility in one PID namespace cannot prove exclusive host CPU access.

## Source and executable identities

- Baseline `src/app.rs`: `bdd5935d87fa594e89e53d175e757b48283f4fb198c8dd25a6c41351c5eec0a6`.
- Candidate `src/app.rs`: `6693c2416462def27690f539c79c21673440c0aeec0d3eebddf3e5eaa078d48d`.
- Validated debug executable: `6f692705c0faaff2c89a2e0b990b6e0f4e53cddaa1d95633392166a9e81a9f32`.

The portable [README](tui-popup/README.md) gives exact build/measurement commands. [`recorded/`](tui-popup/recorded/) contains raw samples, all summaries, source/binary identities, run limits/status, and before/after host/process/hash evidence. Absolute local paths in these historical records are provenance, not portable commands. No binaries, copied production source trees, or prior lost results are included in the repository bundle.
