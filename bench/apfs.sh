#!/bin/sh
# macOS scan benchmark for the APFS work (issue #5): dutype builds against
# du and dua, warm cache. Usage: bench/apfs.sh OUT.md BASE_BIN NEW_BIN TREE...
# Cold cache needs `sudo purge` between runs: add --prepare 'sudo purge'.
set -e
OUT=$1; BASE=$2; NEW=$3; shift 3
: > "$OUT"
for T in "$@"; do
  echo "### $T" >> "$OUT"
  hyperfine -N -i --warmup 2 --runs 10 --style none --export-markdown /tmp/dutype-bench.md \
    -n "dutype base" "$BASE --json $T" \
    -n "dutype new" "$NEW --json $T" \
    -n "du -skx" "/usr/bin/du -skx $T" \
    -n "dua" "dua -x aggregate $T" \
    -n "dua dedup" "dua -x --deduplicate-apfs-clones aggregate $T" >/dev/null
  cat /tmp/dutype-bench.md >> "$OUT"
  echo >> "$OUT"
done
