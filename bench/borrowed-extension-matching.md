# Allocation-free extension popup matching

The popup now compares a borrowed extension suffix against the existing normalized
key instead of allocating a lowercase String for each visited file. Only the
name's ASCII bytes are folded, preserving exact legacy equality for Unicode,
noncanonical query keys, missing suffixes and literal `(none)` suffixes.
Breakdown grouping, accounting, ranking and captured jump identity are unchanged.

## Bounded benchmark

The existing `bench/tui-popup/compare.py` harness compared main
`709c7d283e1898d68cda7bfc411a2735cb14de66` with the matcher patch on Linux,
Rust 1.99.1, using separate ordinary-allocator timing and allocation-count binaries.
Both sides used correct ranking (`--before-buggy` was not used). Fixtures and
verification were outside timing. Seven samples per case, 64 total invocations,
completed on 2026-10-21 at 03:45 UTC. All eight output verification objects matched.

| 100,000 files | Before median | After median | Allocation calls before / after |
| --- | ---: | ---: | ---: |
| Extension, descending sizes | 4.349 ms | 3.458 ms | 102,006 / 2,006 |
| Extension, equal-size ties | 59.425 ms | 58.666 ms | 301,006 / 201,006 |
| Category, descending control | 3.022 ms | 2.821 ms | 2,006 / 2,006 |
| Category, ties control | 60.458 ms | 56.753 ms | 201,006 / 201,006 |

Each extension case removed exactly 100,000 allocations and 300,000 requested
bytes. Retained memory (72,516 requested bytes) and peak-live requested memory
(90,538 bytes) did not change. This reduces allocation traffic, not retained popup
size. Unchanged category controls moved 6–7%, so the timing results do not establish
a universal or precise speedup. Correct tie ranking still owns candidate paths.

Before app SHA256: `6693c2416462def27690f539c79c21673440c0aeec0d3eebddf3e5eaa078d48d`.
Benchmarked candidate app SHA256: `1b285c14444fd1c5de3e956984c1f5571e75afe4434a0674933eb0ad5156d9f0`.
The final patch also incorporates #27's separate deterministic breakdown ordering;
that integration is validated with fresh combined tests, not represented as a new
timing run.

## Correctness

Differential tests cover ASCII controls, Unicode distinctions, dotfiles, trailing
and multiple dots, missing extensions, escaped/lossy names and noncanonical keys.
The popup test compares paths, sizes, filesystem payloads and captured targets for
19 keys in both size modes, including matching-directory rejection and jumps.
An independent Rust 1.88 oracle checked 31,236,096 comparisons without mismatch;
100,000 direct matcher calls allocated zero times. A bounded Linux PTY checked
mixed-case results, distinct Unicode labels and three identity-correct jumps,
including raw/literal display twins, with terminal restoration.
