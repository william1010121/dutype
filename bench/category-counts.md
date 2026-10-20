# Category match counts and bounded top-N reservation

Baseline: `8d9bf2c2255bdafd6765f5ded7edff1abadc5ca1`.

## Scope and cost

Category reporting increments one saturating `u64` counter for each hit in its
existing snapshot walk. Matching directory hits still prune descendants, even
when the top-N heap rejects them. `TopN::accepts` still precedes retained-path
allocation. There is no extra scan, tree materialization, per-entry allocation,
cache-format change, or `Node` storage change. Category `--top 0` already walked
the snapshot before this change; counting does not introduce a new traversal.

The shared top-N helper previously reserved `n + 1` elements immediately. That
overflowed at `usize::MAX`, and smaller enormous limits could request enormous
allocations even for an empty result. Its initial reservation is now
`min(n, 1024)`, with no addition and no change to the requested result limit.
It grows normally when more matches are retained. This can add reallocations
for large lists; the bound is on initial capacity, not results or total memory.

## Bounded before/after measurement

Linux x86-64, Rust stable 1.99.1, release/LTO, hyperfine 2.0.0. The fixture is
20,000 empty PDFs in 100 directories. Each command loads the same warmed CLI
snapshot and renders JSON to hyperfine's discarded stdout. Fixture creation,
initial scanning/cache writing, compilation, and equality checks are outside
the timings. Two rounds, five warmups and 30 measured runs per binary per cell,
reversing before/after ordering between rounds and alternating first order
between cells. Medians below combine all 60 measurements per binary.
The [raw CSV](category-counts-results.csv) includes all 960 successful measured
trials, without outlier removal.

Other coordinated heavy work was paused. Host load averages were
`1.21 / 0.96 / 0.66` before and `1.02 / 0.93 / 0.65` after; this was not an
exclusively reserved host. Peak RSS is the median per-process peak in KiB.
The default full-report control was measured in a separate quiet interval,
with load averages `0.16 / 0.68 / 0.62` before and `0.15 / 0.66 / 0.61` after.

| Report | Top | Before ms | After ms | Change | Before RSS KiB | After RSS KiB |
|:--|--:|--:|--:|--:|--:|--:|
| Category | 0 | 3.503 | 3.613 | +3.2% | 4324 | 4328 |
| Category | 10 | 4.237 | 4.335 | +2.3% | 4324 | 4328 |
| Category | 50 | 4.115 | 4.409 | +7.1% | 4324 | 4328 |
| Category | 1,000 | 6.877 | 6.551 | -4.7% | 5594 | 5592 |
| Category | 1,000,000 | 59.921 | 64.100 | +7.0% | 34568 | 35588 |
| Full default | 20 | 4.494 | 4.706 | +4.7% | 4324 | 4328 |
| Full control | 50 | 4.661 | 4.774 | +2.4% | 4324 | 4328 |
| Full control | 1,000,000 | 38.250 | 39.681 | +3.7% | 14376 | 15392 |

These are not speedup claims or an isolated measurement of counter cost.
Ordinary-limit results move in both directions, including the full-report
control. Large-limit category output was about 4.2 ms slower and used about
1 MiB more peak RSS here; geometric heap growth can cost extra copies and
capacity versus one eager reservation. This benchmark does not establish how
much of that difference is attributable to allocation versus host variation.
The change makes extreme requested limits safe without imposing a result cap.

All existing category JSON fields and retained items matched the baseline
exactly at every measured limit after removing only the two additive fields.
All full-report controls matched exactly. The candidate counted all 20,000
matches even at top zero; top one million returned all 20,000 without truncation.

## Reproduce

Build the baseline and candidate with the same release toolchain and set
`BEFORE` and `AFTER` to their absolute binary paths. Choose a fresh `FIXTURE`
directory outside the repository, then:

```bash
set -eu
export FIXTURE=/tmp/dutype-category-counts-bench
export DUTYPE_CACHE_DIR="$FIXTURE/cache" RAYON_NUM_THREADS=4
python3 - <<'PY'
import os
from pathlib import Path
p = Path(os.environ['FIXTURE'])
p.mkdir(parents=True)  # Fail rather than overwrite an existing fixture.
(p / 'config.toml').write_text('')
for d in range(100):
    sub = p / 'tree' / f'dir-{d:03}'
    sub.mkdir(parents=True)
    for n in range(200):
        (sub / f'file-{n:03}.pdf').touch()
PY
"$BEFORE" --json --cache --config "$FIXTURE/config.toml" \
  --category PDF --top 0 "$FIXTURE/tree" >/dev/null

# Repeat for category top 0, 10, 50, 1000, 1000000; also run full-report
# controls at 50 and 1000000 by omitting --category PDF, and the default
# full report by omitting both --category PDF and --top. Reverse command
# order in a second round, alternating the first order between cells.
hyperfine -N --warmup 5 --runs 30 --export-json "$FIXTURE/result.json" \
  "$BEFORE --json --cache --config $FIXTURE/config.toml --category PDF --top 50 $FIXTURE/tree" \
  "$AFTER --json --cache --config $FIXTURE/config.toml --category PDF --top 50 $FIXTURE/tree"
```

## Regression coverage

Tests cover empty/no-match trees, zero-byte matches, below/exact/over limits,
CLI top zero and `usize::MAX`, heap growth past the initial reservation,
matching-directory pruning and inherited-file exclusion, deterministic ties,
different allocated/deduplicated rankings, CLI/MCP parity, MCP's unchanged
1–500 limit, displayed-only text totals, and fresh/cached unreadable notices.
Portable staged-cache tests verify counts remain snapshot-based until refresh.
