# Deep-tree TUI checks

## Finite snapshot regressions

The default Rust suite contains three filesystem-free snapshot controls:

- `deep_popup_jump_and_size_modes_preserve_snapshot_on_small_stack`: a
  1000-directory chain on a requested 128 KiB worker stack.
- `shallow_popup_jump_and_size_modes_preserve_snapshot_on_small_stack`: the
  same scenario with a 32-directory chain and requested 128 KiB stack.
- `deep_popup_jump_and_size_modes_preserve_snapshot_on_default_stack`: a
  1000-directory chain with `Builder::stack_size` unset.

The fixtures also have a root, sibling file and nested special-directory leaves;
"1000" is the chain parameter, not a claim about an unlimited supported depth.
The controls cover extension aggregation, category and extension popups,
matching-directory pruning, captured-index navigation, size-mode reordering,
shared-record index remapping, and destruction. Their 31 original assertions
remain intact. Clone accounting is supplied as a synthetic completed snapshot:
they test its preservation, not the accounting algorithm or live APFS/reflink
behavior.

Two additional tests compare the iterative sort with the original recursive
implementation on ordinary test stacks. They compare all node, directory,
clone and shared-extent fields, including per-child deduplicated/freeable sizes.
Cases include pruning an unsorted non-clone subtree, clone-marked files and empty
directories, multiple clone siblings, zeros, equal-size/name ties, partial shared
extents, maximum 1000 directory levels, and shallow 1000-sibling groups.

## Windows failure and bounded fix

Before the fix, native Windows tests on Rust 1.99.1 and 1.88.0 overflowed the
requested 128 KiB stack at chain depth 1000 during the first size-mode sort and
location restoration. Phase diagnostics showed fixture/App construction and
extension traversal had completed. The depth-32/small-stack and
depth-1000/default-worker controls passed. This was evidence of a constrained
worker-stack failure, **not** a demonstrated crash on the ordinary production
TUI/main-thread stack.

`app::sort_tree` now uses one remaining-siblings iterator per active ancestor
instead of recursive stack frames. Non-clone roots return before allocation,
and non-clone descendants are still pruned. The existing child sorting and
shared-index remapping are unchanged. Traversal state is O(depth), not O(width);
the width-sized scratch arrays already used by `DirInfo::sort_children` are
separate and unchanged. No unsafe code is needed.

At fixed revision `1476a87f75fad5ed4a7b46ddbd6836b3eee5ad4f`, all six native
Windows toolchain/control combinations passed, including destruction and worker
join. The ordinary Linux/macOS/Windows CI run also passed. The temporary phase
workflow and breadcrumbs were then removed while retaining all three controls.
This is finite 1000-chain/128-KiB evidence, not proof for arbitrary depths,
all stack sizes, or every native filesystem's clone semantics.

Recorded CI provenance in `william1010121/dutype`:

- Baseline phase run: `39557798096`, revision
  `4baa74f2f15c51507e128056fd4447ed5d7d4523`.
- Fixed phase run: `39559456365`; deep/small-stack jobs `119086947663`
  (Rust 1.99.1) and `119086948029` (Rust 1.88.0).
- Fixed ordinary three-platform CI: `39559456383`.

## Bounded sorting cost and resources

The [reproducible benchmark](../bench/deep-mode-sort/README.md) compares the
original and fixed sort implementations using actual production declarations
and methods, including `Category(u8)`. Its registry is a one-category fixture
stub outside the timed sort. The [source manifest](../bench/deep-mode-sort/recorded/source-manifest.json)
records source/binary hashes, compiler flags and layout sizes; the
[provenance record](../bench/deep-mode-sort/recorded/provenance.json) identifies
exact revisions. [Raw measurements](../bench/deep-mode-sort/recorded/raw.ndjson)
and their [summary](../bench/deep-mode-sort/recorded/summary.json) are retained.
An earlier zero-sized-Category prototype was excluded, not mixed into these results.

This x86_64 Linux/Rust 1.99.1 release microbenchmark has three warmups and fifteen
samples per case, alternating candidate order between cases. Each sample
averages a fixed batch of dedup+du sort pairs. Fixture construction, digesting,
output and destruction are outside timing. All before/after snapshot digests
match. Separate binaries measure ordinary latency, whole-process RSS, and safe
iterator-capacity instrumentation. There are no timing-threshold tests.

| Synthetic snapshot | Before pair median | After pair median | Change | Peak iterator capacity | Before / after peak RSS range (KiB) |
| --- | ---: | ---: | ---: | ---: | --- |
| Non-clone root, 1000 files | 4.483 ns | 4.396 ns | -2.0% | 0 B | 912 / 912 |
| Ordinary, 16 dirs × 16 files | 6.877 µs | 6.986 µs | +1.6% | 64 B | 912 / 912 |
| Wide flat, 50000 files | 11.954 ms | 11.473 ms | -4.0% | 16 B | 6060–6064 / 6060–6064 |
| Wide shallow, 1000 dirs × 8 files | 160.364 µs | 162.774 µs | +1.5% | 64 B | 1936 / 1936 |
| Deep, 1000 directory levels | 32.857 µs | 34.215 µs | +4.1% | 16384 B | 1296 / 1168–1296 |

RSS uses three fresh child processes per candidate/case and includes the entire
process, including fixture creation and teardown. It is coarse and does not
isolate traversal allocations. Iterator capacity is measured independently:
it is 16 bytes per reserved frame on this target and zero for a non-clone root.
These finite, short sort-only measurements are not full TUI latency or a broad
speedup/regression guarantee; the measured deep difference is about 1.36 µs per
pair. Known concurrent builds/tests were idle, but that cannot prove exclusive
host CPU use.

## Optional Linux PTY check

An optional Linux PTY check exercises a real temporary 1000-directory fixture:

```sh
cargo build
python3 tests/tui_deep_tree.py --binary target/debug/diskbytype --output /tmp/diskbytype-deep-proof
```

Use `--depth 350` for the smaller finite fixture. The helper imports only the
VT screen decoder from `tui_result_jump.py`; it has no Python package dependencies.
It creates one 8 MiB PDF, tests both category and extension result jumps, resizes
to 1×1 and back, toggles du/dedup sizes, checks Back selection, and verifies clean
terminal restoration. It isolates PATH/config/home, never sends reveal/open/copy
keys, reaps only its own child processes, and removes tracked fixture paths in
reverse order rather than recursively. ANSI output and decoded frames are saved
for diagnosis even if a session fails.

Completion is condition-based with bounded failure timeouts, not a performance
threshold. Linux inotify verifies that result jumps do not open/read fixture
paths; metadata-only syscalls are not measured. Ordinary files do not establish
actual shared-extent/clone behavior, which the synthetic Rust test covers only
at the snapshot-state level. The PTY helper is intentionally not in default CI.
