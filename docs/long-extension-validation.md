# Long custom extensions: validation

## Scope and compatibility

Custom filename suffixes longer than 16 UTF-8 bytes now honor their configured
category instead of silently falling back to Other. ASCII case is ignored;
non-ASCII bytes match exactly. The existing short-extension stack/hash path is
unchanged. A sorted vector is derived from final normalized rules after overrides
and category-index shifts; the authoritative extension map remains intact for
mixed-category inventory eligibility.

Cache fingerprints add a classification revision only when final long rules
exist, including long rules assigned to Other. Same-version stale Other snapshots
are rebuilt; configurations without long rules retain their historical fingerprint.
The built-in configuration text is unchanged.

Integrated onto main `e7a9fbaa7ceb6dac11678b3eed7dd1d22338ce8d`, including the
optional-config error handling and mixed-category inventory fixes. The benchmarked
registry implementation, including fingerprint finalization and classification,
is byte-identical to the integrated implementation. Only already-landed config
loading differs from the benchmark source; timings below were not rerun or relabeled
as measurements of the integrated tree.

## Correctness checks

Rust 1.99.1 formatting, all-target Clippy with warnings denied, and full tests;
Rust 1.88.0 full tests. Both integrated suites passed 192 tests, with two existing
ignored tests. Coverage includes ASCII 15/16/17-byte and UTF-8 16/17-byte
boundaries; ASCII folding versus non-ASCII case differences; leading-dot normalization;
dotfiles/trailing/multiple dots; overrides; Other index shifts; deterministic rule
order; long-only mixed inventory eligibility; directory precedence; cache rule
add/remove; and old same-version cache rejection, rebuild, and reuse.

A genuine old-binary cache upgrade also verifies byte-identical short-rule cache
reuse and corrected long-rule classification without changing allocated byte totals.
The combined config-loading/mixed-query checks cover implicit and explicit configs,
CLI/MCP option parity, paginated continuation binding/replay in du/dedup modes,
and config errors before scans. All 12 cases passed across stable/MSRV binaries,
explicit/environment/default config sources, and short/long rules. Clone-capable
snapshots were seeded so du/dedup comparisons did not require a cache upgrade;
retained MCP snapshots were primed before exact metadata comparisons.
Local validation is Linux x86_64, not a native macOS/Windows performance claim.

## Benchmark method

Baseline `708e08e3a668ad02f5aba021a3292b7a27e61b1f`; candidate feature
`5d3acf5981803c837e689bfb3c0b09be4af7f225`. Both use package version 0.1.4,
Rust 1.99.1 optimized builds and identical dependencies. Hyperfine 2.0.0,
Linux x86_64, no concurrent known benchmark/build workload, no CPU pinning.

The standalone classifier harness uses exact source, black-box inputs, and a
counting allocator. Config parsing/registry creation is outside the internal timed
loop. Every invocation performs 2 million lookups. Each case/binary has two warmups,
seven Hyperfine samples and seven additional internal-loop/allocation samples.
Hyperfine includes process/setup time and records wall/CPU time and peak RSS.

Use 0/1/10/100 distinct normalized long rules, `longextensionrule000` through
`longextensionrule099`. First/last hits use ASCII-uppercase suffixes in sorted order;
the same-length shared-prefix miss is `LONGEXTENSIONRULE999`. Short controls are
`PDF` (hit) and `zzy` (miss). Count-zero long cases are unconfigured suffixes.
Micro count zero uses empty user config; scan count zero creates an empty Long
category. Each baseline/candidate pair uses identical input/configuration.

End-to-end scans use 10,000 zero-length files, explicit config, one Rayon thread,
no disk cache, JSON output with top=1, warm filesystem caches, two warmups and
seven samples per binary/case. These fixtures are not a real-world storage mix.
Baseline long hits incorrectly return Other; candidate hits correctly return Long,
so those comparisons intentionally measure different classification behavior.
Expected categories/checksums and zero loop allocations are asserted.

## Measured medians

All per-sample wall/CPU/RSS measurements and internal-loop counters are in
[the raw results CSV](../bench/long-extension-results.csv). Times below are internal
nanoseconds per classification and whole-process milliseconds per 10k-file scan.

| Long rules | Case | Before ns | After ns | Before ms | After ms |
| ---: | --- | ---: | ---: | ---: | ---: |
| 0 | first | 13.48 | 13.86 | 20.24 | 19.35 |
| 0 | last | 12.15 | 13.57 | 19.66 | 21.14 |
| 0 | miss | 12.19 | 14.63 | 17.98 | 17.64 |
| 0 | short | 19.02 | 19.37 | 17.12 | 18.91 |
| 0 | short_miss | 18.74 | 16.31 | 15.96 | 17.76 |
| 1 | first | 14.09 | 17.89 | 18.98 | 20.03 |
| 1 | last | 13.73 | 19.08 | 20.55 | 18.31 |
| 1 | miss | 12.87 | 18.33 | 19.43 | 18.14 |
| 1 | short | 18.20 | 17.82 | 17.54 | 19.44 |
| 1 | short_miss | 17.45 | 15.74 | 18.50 | 20.41 |
| 10 | first | 12.52 | 17.65 | 18.06 | 20.90 |
| 10 | last | 12.64 | 51.17 | 19.91 | 19.70 |
| 10 | miss | 12.42 | 59.04 | 19.49 | 19.52 |
| 10 | short | 17.86 | 18.36 | 18.06 | 18.63 |
| 10 | short_miss | 18.65 | 19.41 | 18.12 | 17.33 |
| 100 | first | 13.04 | 17.44 | 17.08 | 17.76 |
| 100 | last | 13.78 | 412.98 | 19.82 | 24.10 |
| 100 | miss | 13.33 | 415.94 | 20.80 | 26.85 |
| 100 | short | 17.12 | 18.38 | 18.07 | 18.31 |
| 100 | short_miss | 20.03 | 23.43 | 19.32 | 18.53 |

The deliberate linear fallback cost is visible: at 100 long rules, late hits and
same-prefix misses take about 413/416 ns, and scan medians increase **22% / 29%**.
First hits are about 17 ns. This is a bounded correctness tradeoff, not a scan-speed
optimization. Larger custom rule sets can cost more for late hits and misses.

Short-control ratios ranged 0.87–1.17 for classifier timing and 0.96–1.11 for scans.
The short source path is unchanged, but the noisy measurements do **not** establish
zero measured overhead or a speedup.

All 280 internal samples, covering 560 million lookups, recorded zero allocations,
deallocations, or reallocations during classification. Registry inline storage grows
24 bytes; additional retained heap for 0/1/10/100 rules was 0/148/712/6096 bytes.
This is one-time registry setup storage, not per-file memory.
