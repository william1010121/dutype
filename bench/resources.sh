. ~/bench/tools.sh
# Average user/sys CPU seconds and peak RSS over 5 warm runs.
T=$1
measure() {
  name=$1; shift
  "$@" >/dev/null 2>&1; "$@" >/dev/null 2>&1   # warm up
  for i in 1 2 3 4 5; do /usr/bin/time -f "%U %S %M" "$@" 2>&1 >/dev/null | tail -1; done |
    awk -v n="$name" "{u+=\$1; s+=\$2; if (\$3>m) m=\$3} END {printf \"| %s | %.2f | %.2f | %.2f | %.0f |\n\", n, u/NR, s/NR, (u+s)/NR, m/1024}"
}
echo "| Tool | user (s) | sys (s) | CPU total (s) | Peak RSS (MiB) |"
echo "|:---|---:|---:|---:|---:|"
measure dutype $DUTYPE --report $T
measure dua dua -x aggregate $T
measure gdu gdu -npsx $T
measure dust dust -x -d 0 -P $T
measure "ncdu -t16" ncdu -x -0 -t 16 -o /dev/null $T
measure ncdu ncdu -x -0 -o /dev/null $T
measure du du -skx $T
