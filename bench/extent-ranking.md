# Zero-memory same-directory extent ranking

Base: `49300f8c207e6fba4faa4b442735fb4c966ee8f5` (PR18).

## Scope

Only the representative-ID comparator in `extent_dups` changes. For members in
one directory, resolve that directory once instead of reconstructing and
walking two identical paths; root-directory members use the root directly.
Compare the existing file nodes with the existing name/lossy ordering, then
inode. Different-directory members still use the original `compare` unchanged.
The standard-library stable sort and the original comparator remain intact.

There is no pointer/name cache, copied name, new `Node` field, unsafe code, or
ordinary scan hot-path change. Existing path scratch is reused; there is no new
O(members) or O(directories) storage. Clone-family and hard-link comparisons are
unchanged. This is a narrow comparator simplification, **not a cancellable sort
or a cancellation-latency guarantee**.

## Validation

Linux, Rust stable 1.99.1 and MSRV 1.88.0: 78 unit tests and 14 integration tests
passed, with the existing two manual benchmarks ignored. Strict stable clippy
and `cargo fmt --check` passed. Three added tests cover:

- All-pairs and exact stable-ID differential comparison with the untouched
  original comparator: nested paths, same/different directories, valid/lossy
  display-name collisions, equal names, different and equal inodes, and
  deliberately permuted `Shared` slots versus child indices. Different member
  IDs are explicitly asserted to compare equal; rotated/reversed inputs retain
  exactly the original stable order.
- A 2,049-directory path, exercising the non-root same-directory shortcut.
- Three extent holders among 1,025 members, checking all held/duplicate credits.

The complete accounting harness matched every baseline fingerprint in every
completed trial. Hashes cover node/category aggregates, all child dedup/freeable
values, and every Shared identity/refcount/duplicate/extent credit:

|Members|Clone|Extent|
|---:|---|---|
|100,000|`f21c1dc6c005ed19`|`8b6ff4fcd1211604`|
|1,000,000|`71e96410dfb979e9`|`a8ff9cbb5337b545`|

## Full-accounting measurements

Five sequential release/LTO trials per cell, baseline then candidate, using the
unchanged `bench/account-cancellation.rs`. Fixture construction and fingerprints
are outside operation timing. Local heavy work was explicitly paused, but the
host was not exclusively reserved. Median milliseconds:

|Members|Kind|Mode|Baseline|Shortcut|Change|
|---:|---|---|---:|---:|---:|
|100,000|clone|retained|20.010|21.692|+8.4%|
|100,000|clone|scan|26.268|34.993|+33.2%|
|100,000|extent|retained|112.214|100.533|-10.4%|
|100,000|extent|scan|102.489|83.064|-19.0%|
|1,000,000|clone|retained|250.832|301.044|+20.0%|
|1,000,000|clone|scan|465.626|463.901|-0.4%|
|1,000,000|extent|retained|2188.349|2314.932|+5.8%|
|1,000,000|extent|scan|2347.994|2126.977|-9.4%|

**Do not claim a universal speedup.** The unchanged clone controls moved
materially, and the million-member retained extent case regressed in this pair.
The evidence supports removing redundant work with no new cache cost, but does
not establish a precise improvement across workloads. Full-harness peak RSS was
532,544 KiB baseline and 532,580 KiB candidate; the total includes fixtures,
other accounting buffers, allocator reuse, and destruction.

## Ranking-only workload checks

The isolated research harness used the production comparator functions and
u32 IDs, five trials, exact sorted-ID hashes, and total rank time including any
cache creation/destruction. The shortcut and original matched hashes for every
case. These original/shortcut main-matrix runs were in **different quiet
windows**, so small differences are not precise paired speedup estimates.
Median milliseconds for 1M members:

|Input pattern|Flat original|Flat shortcut|Nested original|Nested shortcut|
|---|---:|---:|---:|---:|
|random|2244.874|1842.301|587.272|477.593|
|sorted|11.496|10.437|236.795|239.775|
|reverse|10.755|10.739|216.258|217.072|
|equal|11.718|8.605|239.999|234.434|
|almost sorted|284.171|251.974|471.130|384.255|

Nested means 1,000 files per depth-two directory with scrambled directory
names; pattern names describe file-name order, not global directory order.
Almost sorted swaps each thousandth adjacent pair. Additional checks covered
one/1%/10%/25% participation among 1M members and one/two files per directory
at 100k members. The one-ranked-ID shortcut costs approximately zero, without
allocating an all-member cache.

## Alternatives withheld

Cached immutable file references were much faster on dense random fixtures,
but unconditionally needed one pointer per member plus directory references,
and regressed sparse, some ordered, and one-file-per-directory cases. Combining
that cache with a cooperative 16k-run merge sort improved dense cancellation
samples but regressed nested random and low-locality workloads. Both remain
isolated research, not part of this patch; no density/locality heuristics were
introduced to hide those cases.

## Reproduce

Run the existing manual harness with the same release toolchain on baseline
and candidate: `cargo test --release synthetic_account_cancellation -- --ignored --nocapture`.
Run ordinary checks with `cargo test`, `cargo +1.88.0 test`,
`cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.

The timed test binary also contained research-only ignored benchmarks and
helpers; those were removed from the final candidate. The production comparator
and extent-ranking call are unchanged, and the final cleaned source passed the
complete suites above. Raw research outputs and immutable executables were
preserved separately; they are not required for the production patch.
