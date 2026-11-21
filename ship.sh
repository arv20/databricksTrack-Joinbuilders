#!/bin/sh
# Verify a pipeline candidate and, only if every check passes, commit and push it.
# Usage: ./ship.sh [CANDIDATE]   (default: solution/pipeline/pipeline.py, the other session's working copy)
#        NOPUSH=1 ./ship.sh       verify + commit only
#        AMD64=1 ./ship.sh        also build the linux/amd64 scorer image and run it on dev (slow, emulated)
# Gates: 36 fixtures, exact match with dev-answer, reference/cross_check.sh on 9 seeds.
set -eu
cd "$(dirname "$0")"
export PATH="$HOME/.docker/bin:$PATH"
PY=.venv/bin/python
SRC=${1:-solution/pipeline/pipeline.py}
CAND=$(mktemp -t cand).py
cp "$SRC" "$CAND"
SHA=$(shasum "$CAND" | cut -c1-8)
echo "== candidate $SRC ($SHA)"
if cmp -s "$CAND" pipeline/pipeline.py 2>/dev/null; then echo "identical to the shipped pipeline; nothing to do"; exit 0; fi

echo "== fixtures"
$PY scripts/run_fixtures.py fixtures -- $PY "$CAND" > /tmp/ship_fx.txt 2>&1 || { grep -v '^ok' /tmp/ship_fx.txt | cut -c1-300 | tail -20; echo "BLOCKED: fixtures"; exit 1; }
tail -1 /tmp/ship_fx.txt

echo "== dev set"
rm -rf out && mkdir -p /tmp/ship_scratch
start=$(date +%s)
DATA_DIR=dev OUTPUT_DIR=out SCRATCH_DIR=/tmp/ship_scratch NCPU=8 MEM_GB=10 $PY "$CAND" >/dev/null 2>/tmp/ship_dev.err || { tail -5 /tmp/ship_dev.err; echo "BLOCKED: dev run crashed"; exit 1; }
echo "dev run: $(( $(date +%s) - start )) s"
(cd scripts && ../$PY compare.py ../out ../dev-answer | tail -1) | grep -q '^MATCH' || { echo "BLOCKED: dev differs"; exit 1; }
echo MATCH; rm -rf out /tmp/ship_scratch

echo "== cross-seed"
PIPELINE="$CAND" reference/cross_check.sh > /tmp/ship_seeds.txt 2>&1 || { grep -E 'DIFFER|CRASH|failed' /tmp/ship_seeds.txt; echo "BLOCKED: seeds (full output in /tmp/ship_seeds.txt)"; exit 1; }
grep -c MATCH /tmp/ship_seeds.txt | sed 's/$/ seeds match/'

if [ "${AMD64:-0}" = 1 ]; then
  echo "== amd64 image"
  cp "$CAND" pipeline/pipeline.py
  docker buildx build -q --platform linux/amd64 --load -t shopping-sessions:latest . >/dev/null
  PLATFORM=linux/amd64 CPUS=8 MEM=10g scripts/docker_run.sh dev out >/dev/null 2>&1 || { git checkout -q pipeline/pipeline.py 2>/dev/null || true; echo "BLOCKED: amd64 run failed"; exit 1; }
  (cd scripts && ../$PY compare.py ../out ../dev-answer | tail -1) | grep -q '^MATCH' || { echo "BLOCKED: amd64 dev differs"; exit 1; }
  echo "amd64 MATCH"; rm -rf out
fi

cp "$CAND" pipeline/pipeline.py
rm -f "$CAND"
git add -A
git commit -q -m "Ship pipeline $SHA (fixtures 36/36, dev MATCH, 9 seeds MATCH)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
echo "== committed $(git rev-parse --short HEAD)"
if [ "${NOPUSH:-0}" != 1 ]; then
  git push -q origin HEAD:main
  echo "== pushed. Submit commit $(git rev-parse HEAD) on the track page (or leave SHA empty for newest on main)."
fi
