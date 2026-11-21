#!/bin/sh
# Diff the real pipeline against reference/reference.py on several seeds.
# Usage: reference/cross_check.sh [EVENTS] [SEED...]
# Defaults: 300000 events; seeds 2-7 plus 15, 62, 249 (windows holding 2028-02-29).
# Works on a snapshot of the pipeline, so edits in progress don't break a run.
# Each dataset is deleted right after use (the disk is nearly full).
set -u
cd "$(dirname "$0")/.."
PY=.venv/bin/python
GEN=generator/target/release/generate
PIPE=${PIPELINE:-solution/pipeline/pipeline.py}
EVENTS=${1:-300000}
[ $# -gt 0 ] && shift
SEEDS=${*:-2 3 4 5 6 7 15 62 249}
CLICKS=$((EVENTS / 4))
WORK=seeds
mkdir -p "$WORK"
cp "$PIPE" "$WORK/pipeline_snapshot.py"
echo "pipeline snapshot: $PIPE ($(shasum "$PIPE" | cut -c1-8))"
status=0
for s in $SEEDS; do
  d="$WORK/$s"
  rm -rf "$d"
  $GEN --seed "$s" --events "$EVENTS" --clicks "$CLICKS" --output "$d/data" >/dev/null 2>&1 || { echo "seed $s: generate failed"; status=1; continue; }
  win=$(tr -d '\n ' < "$d/data/manifest.json")
  mkdir -p "$d/scratch"
  if ! DATA_DIR="$d/data" OUTPUT_DIR="$d/pipe" SCRATCH_DIR="$d/scratch" NCPU=8 MEM_GB=8 \
       $PY "$WORK/pipeline_snapshot.py" >/dev/null 2>"$d/pipe.err"; then
    echo "seed $s: PIPELINE CRASHED"; tail -5 "$d/pipe.err"; status=1; rm -rf "$d"; continue
  fi
  DATA_DIR="$d/data" OUTPUT_DIR="$d/ref" $PY reference/reference.py >/dev/null 2>"$d/ref.err" || { echo "seed $s: REFERENCE CRASHED"; tail -5 "$d/ref.err"; status=1; rm -rf "$d"; continue; }
  result=$(cd starter-duckdb/scripts && ../../$PY compare.py "../../$d/pipe" "../../$d/ref" 2>&1)
  if echo "$result" | tail -1 | grep -q '^MATCH'; then
    echo "seed $s: MATCH  $win"
  else
    echo "seed $s: DIFFER $win  (A = pipeline, B = reference)"
    echo "$result" | head -40
    status=1
  fi
  rm -rf "$d"
done
rm -f "$WORK/pipeline_snapshot.py"
exit $status
