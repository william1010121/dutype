#!/bin/sh
# macOS scan benchmark for the APFS work (issue #5): dutype builds against
# du and dua, warm cache. Usage: bench/apfs.sh OUT.md BASE_BIN NEW_BIN TREE...
# Cold cache needs `sudo purge` between runs: add --prepare 'sudo purge'.
set -e
OUT=$1; shift
# hyperfine splits each command string like a shell: quote every path.
q() { printf "'%s'" "$(printf %s "$1" | sed "s/'/'\\\\''/g")"; }
BASE=$(q "$1"); NEW=$(q "$2"); shift 2
: > "$OUT"
for P in "$@"; do
  echo "### $P" >> "$OUT"
  T=$(q "$P")
  hyperfine -N -i --warmup 2 --runs 10 --style none --export-markdown /tmp/dutype-bench.md \
    -n "dutype base" "$BASE --json $T" \
    -n "dutype new" "$NEW --json $T" \
    -n "du -skx" "/usr/bin/du -skx $T" \
    -n "dua" "dua -x aggregate $T" \
    -n "dua dedup" "dua -x --deduplicate-apfs-clones aggregate $T" >/dev/null
  cat /tmp/dutype-bench.md >> "$OUT"
  echo >> "$OUT"
done
