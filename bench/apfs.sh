#!/bin/sh
# macOS scan benchmark for the APFS work (issue #5): diskbytype builds against
# du and dua, warm cache. Usage: bench/apfs.sh OUT.md BASE_BIN NEW_BIN TREE...
# Cold cache needs `sudo purge` between runs: add --prepare 'sudo purge'.
# A failing command aborts the run. IGNORE_FAILURES=1 keeps going (du exits
# 1 on unreadable entries, e.g. in ~) and marks the table as such.
set -e
IGN=; [ -n "$IGNORE_FAILURES" ] && IGN=-i
OUT=$1; shift
# hyperfine splits each command string like a shell: quote every path.
q() { printf "'%s'" "$(printf %s "$1" | sed "s/'/'\\\\''/g")"; }
BASE=$(q "$1"); NEW=$(q "$2"); shift 2
: > "$OUT"
for P in "$@"; do
  echo "### $P" >> "$OUT"
  T=$(q "$P")
  hyperfine -N $IGN --warmup 2 --runs 10 --style none --export-markdown /tmp/diskbytype-bench.md \
    -n "diskbytype base" "$BASE --json $T" \
    -n "diskbytype new" "$NEW --json $T" \
    -n "du -skx" "/usr/bin/du -skx $T" \
    -n "dua" "dua -x aggregate $T" \
    -n "dua dedup" "dua -x --deduplicate-apfs-clones aggregate $T" >/dev/null
  cat /tmp/diskbytype-bench.md >> "$OUT"
  [ -z "$IGN" ] || echo "(non-zero exit codes ignored)" >> "$OUT"
  echo >> "$OUT"
done
