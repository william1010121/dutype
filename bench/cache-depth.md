# Bounded iterative cache traversal

Measured on Linux x86_64 with Rust 1.99.1, release profile (LTO), against the
recursive node codecs from `a097a1e2a2f2716ede166a155e9440f2565fa6dd`. The v9 byte
layout is unchanged. This is a depth-correctness change, not a claim that every
cache operation becomes faster.

## Scope and bounds

Both codecs support at most 16,384 simultaneously open directories, including
a directory root. Files consume no directory frame: 16,384 directories ending
in a file are supported, as is an empty 16,384th directory. A 16,385th directory
is rejected by both codecs. An over-limit save returns `InvalidData`, preserves
any previous snapshot, and removes its temporary file.

The writer keeps one child cursor per open directory, not one task per sibling.
The reader assembles children bottom-up and allocates a directory's category
totals only after its children are complete. On x86_64 the logical frame payload
is 16 bytes for writing and 72 bytes for reading, at most 256 KiB and 1.125 MiB
respectively at this budget. This is **not** a total-memory bound: input bytes,
names, completed child trees, category arrays, vector capacities and allocator
overhead are additional. Valid broad trees still require memory proportional to
the decoded content. The budget is a supported nesting limit, not a universal
claim about operating-system path limits.

## Timing method

A temporary test module compiled the old node methods alongside the new ones,
using unchanged varint/field helpers and the same fixture objects and byte
buffers. Fixtures were synthesized in memory; filesystem scanning, disk I/O,
snapshot headers, private-table matching and clone accounting were not timed.

Every directory was named `d`, had 3 own bytes and category 0. Each ordinary
file was named `file`, had 4,096 bytes and category 0. There were no sharing,
private-size or extent records in these throughput fixtures:

- Shallow: one directory with 32 files, 299 serialized bytes; 20,000 iterations.
- Wide: one directory with 50,000 files, 450,014 bytes; 100 iterations.
- Branching: a root with 200 directories, each with 100 files, 182,213 bytes;
  100 iterations.

Seven rounds alternated old/new execution order. Each write reused a Vec with
capacity for the encoded tree, clearing it between iterations. Each read started
a fresh reader, decoded a fresh tree, and dropped it inside the timed interval.
Inputs and outputs passed through `std::hint::black_box`. Reported values are
medians of seven per-round times divided by iteration count. Other coordinated
work was idle; the host reported nine allowed CPUs and load averages of about
0.62 before / 0.68 after. This is a local microbenchmark, not a cross-platform
performance guarantee.

| Fixture | Write before → after | Change | Read + drop before → after | Change |
| --- | ---: | ---: | ---: | ---: |
| Shallow | 0.772 → 0.731 µs | −5.3% | 1.274 → 1.298 µs | +1.8% |
| Wide | 1.124 → 1.084 ms | −3.5% | 2.583 → 2.670 ms | +3.4% |
| Branching | 0.488 → 0.469 ms | −4.0% | 1.251 → 1.275 ms | +1.9% |

Reading has a small measured cost for explicit traversal and bottom-up
completion; writing is modestly faster in these fixtures. A preliminary
completed-node staging loop had a 13–23% read regression and was replaced
before these final measurements.

Separate processes encoded and decoded the wide fixture once, retaining the
original tree, bytes and decoded tree together. Linux child-process peak RSS
was 9,984 KiB before and 9,708 KiB after. These are comparable process-level
figures; the small difference is allocator/process noise, not evidence of a
memory reduction. The timing process's combined peak RSS is not a per-codec
memory measurement.

Raw rounds and RSS probe output: [cache-depth-results.txt](cache-depth-results.txt).
The temporary legacy decoder and timing probes are not part of production tests.

## Correctness coverage

Permanent tests cover literal legacy v9 node bytes with all node flags and
clone/extent records; actual scanned 257/350-directory cache round trips; full
cache loading with deep clone/private/extent metadata on a 128 KiB stack; exact
budget acceptance/rejection, successful destruction and partial-decode cleanup;
and preservation of a previous cache after an over-budget save. The existing
CLI test now also asserts a real warm-cache hit for its 1,500 nested directories
on Linux (400 on macOS), rather than merely surviving a fallback rescan. A small bounded
recursive traversal oracle shares field-validation helpers and checks malformed
and truncated traversal behavior; it is not independent validation evidence.

Separately, the unmodified baseline node decoder and writer were compared with
the final traversal over 118,362 bounded truncation/mutation cases and mixed
branching fixtures. They agreed on acceptance and decoded fields, and shallow
encoded bytes matched. This check preserves established validation behavior;
it is not a broad cache-format audit.

Validation on the final source: stable 1.99.1 `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` passed. Full tests on stable 1.99.1
and MSRV 1.88.0 each passed 84 unit and 14 integration tests; two pre-existing
manual benchmark tests remain ignored. Formatting/clippy used stable only;
MSRV formatting/clippy components were not installed or changed. macOS and
Windows execution remains for cross-platform CI.
