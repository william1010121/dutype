. ~/bench/tools.sh
T=$1; MODE=$2; OUT=$3
if [ "$MODE" = cold ]; then
  OPTS="--runs 5 --prepare \"sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null\""
else
  OPTS="--warmup 2 --runs 10"
fi
eval hyperfine -N $OPTS --export-markdown $OUT --style none \
  -n diskbytype       "\"$DUTYPE --report $T\"" \
  -n dua          "\"dua -x aggregate $T\"" \
  -n gdu          "\"gdu -npsx $T\"" \
  -n dust         "\"dust -x -d 0 -P $T\"" \
  -n ncdu         "\"ncdu -x -0 -o /dev/null $T\"" \
  -n ncdu-t16     "\"ncdu -x -0 -t 16 -o /dev/null $T\"" \
  -n du           "\"du -skx $T\""
