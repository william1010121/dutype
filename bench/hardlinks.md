# Stable hard-link attribution benchmark

Linux x86_64 cloud workspace, warm filesystem cache, Rust 1.99.1, unchanged
Cargo.lock and release profile, `RAYON_NUM_THREADS=4`, hyperfine 2.0.0. Baseline:
main `af7d45193ccdb32f109e6761e1f26e6621e56604`. Each measurement launches the
complete CLI process with the same empty config and `--report` arguments.
Only synthetic temporary data was used.

`bench/hardlinks.py` creates two trees of 160,000 file entries in 80 sibling
directories. Ordinary files each contain 4 KiB. The hard-link tree contains
20,000 unique 4-KiB inodes, each with eight links spread across directories.
Both versions produce 655,360,000 ordinary bytes and 81,920,000 hard-link bytes,
matching `du -skx` on this filesystem (directories allocate no blocks here).

## Before/after

Twenty measured runs after three warmups; mean ± standard deviation. Memory
is hyperfine's full-process `memory_peak_resident`, not a retained-tree estimate.
Percentages compare the corresponding before/after fixture, not the first
command in hyperfine's multi-command output.

| Fixture | Before | After | Time change | Before peak RSS | After peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ordinary | 74.45 ± 6.22 ms | 73.52 ± 3.92 ms | -1.2% | 14.44 MiB | 14.43 MiB |
| Hard links | 63.97 ± 5.63 ms | 71.82 ± 3.17 ms | +12.3% | 14.91 MiB | 25.01 MiB |

A separate paired 20-run hard-link measurement gave 70.97 ± 6.26 ms before
and 87.43 ± 7.76 ms after (+23.2%), with 14.92 versus 25.08 MiB peak RSS.
This shared machine's timing varies; the cost is real, not a claimed speedup.
The ordinary-file difference is within run variance. Hard-link peak RSS rises
about 68% (roughly 10 MiB here).

The initial implementation sorted a second full record for every link and
measured 69.9 → 103.6 ms, 14.9 → 31.4 MiB. The retained implementation reduces
links directly into a hash map containing only two compact candidates per
inode. It avoids a second per-link record array and its O(H log H) group sort.

## Space and hot-path tradeoff

Hard-link scans temporarily retain the existing 48-byte `Shared` record per
link, plus two 16-byte candidate locations per distinct inode and hash-table /
affected-directory overhead. Default scans release these records after
normalization; the allocator need not return the process RSS to its baseline.
With `--clones`, the shared records are already needed for clone/freeable
accounting. Ordinary files gain no syscall, lock, allocation, or Node field;
trees without hard links skip the attribution pass entirely.

## Reproduce

```sh
python3 bench/hardlinks.py /tmp/dutype-bench-new
# Build both revisions with cargo build --release and copy the executables.
RAYON_NUM_THREADS=4 hyperfine --warmup 3 --runs 20 \
  --export-json /tmp/hardlinks.json \
  '/tmp/dutype-before --report --config /tmp/dutype-bench-new/config.toml /tmp/dutype-bench-new/plain' \
  '/tmp/dutype-after --report --config /tmp/dutype-bench-new/config.toml /tmp/dutype-bench-new/plain' \
  '/tmp/dutype-before --report --config /tmp/dutype-bench-new/config.toml /tmp/dutype-bench-new/links' \
  '/tmp/dutype-after --report --config /tmp/dutype-bench-new/config.toml /tmp/dutype-bench-new/links'
```
