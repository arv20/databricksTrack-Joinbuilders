#!/bin/sh
# Verify the Rust pipeline in rust/ and, only if every check passes, commit and push it.
# Usage: ./ship.sh            fixtures + dev + 9 seeds vs reference, then commit + push
#        NOPUSH=1 ./ship.sh   verify + commit only
#        AMD64=1 ./ship.sh    also build the real linux/amd64 image and run dev + fixtures in it (slow, emulated)
# The Python/DuckDB fallback (Dockerfile.python, pipeline/) is no longer what the root Dockerfile builds.
set -eu
cd "$(dirname "$0")"
export PATH="$HOME/.docker/bin:$PATH"
PY=.venv/bin/python
BIN=rust/target/release/rproto

echo "== build"
(cd rust && cargo build --release -q)
SHA=$(cat rust/Cargo.toml rust/Cargo.lock rust/src/*.rs | shasum | cut -c1-8)
echo "rust source $SHA"
if [ -n "${EXPECT:-}" ] && [ "${SHA#"$EXPECT"}" = "$SHA" ]; then echo "BLOCKED: source is $SHA, expected $EXPECT"; exit 1; fi

echo "== fixtures"
$PY scripts/run_fixtures.py fixtures -- $BIN > /tmp/ship_fx.txt 2>&1 || { grep -v '^ok' /tmp/ship_fx.txt | cut -c1-300 | tail -20; echo "BLOCKED: fixtures"; exit 1; }
tail -1 /tmp/ship_fx.txt

echo "== dev set"
rm -rf out
start=$(date +%s)
DATA_DIR=dev OUTPUT_DIR=out NCPU=8 $BIN >/dev/null 2>/tmp/ship_dev.err || { tail -5 /tmp/ship_dev.err; echo "BLOCKED: dev run crashed"; exit 1; }
echo "dev run: $(( $(date +%s) - start )) s"
(cd scripts && ../$PY compare.py ../out ../dev-answer | tail -1) | grep -q '^MATCH' || { echo "BLOCKED: dev differs"; exit 1; }
echo MATCH; rm -rf out

echo "== cross-seed vs reference"
PIPELINE="$BIN" reference/cross_check.sh > /tmp/ship_seeds.txt 2>&1 || { grep -E 'DIFFER|CRASH|failed' /tmp/ship_seeds.txt; echo "BLOCKED: seeds (full output in /tmp/ship_seeds.txt)"; exit 1; }
grep -c MATCH /tmp/ship_seeds.txt | sed 's/$/ seeds match/'

if [ "${AMD64:-0}" = 1 ]; then
  echo "== amd64 image"
  docker buildx build -q --platform linux/amd64 --load -t shopping-sessions:latest . >/dev/null
  PLATFORM=linux/amd64 CPUS=8 MEM=10g scripts/docker_run.sh dev out >/dev/null 2>&1 || { echo "BLOCKED: amd64 run failed"; exit 1; }
  (cd scripts && ../$PY compare.py ../out ../dev-answer | tail -1) | grep -q '^MATCH' || { echo "BLOCKED: amd64 dev differs"; exit 1; }
  echo "amd64 MATCH"; rm -rf out
fi

git add -A
git commit -q -m "Ship Rust pipeline $SHA (fixtures 36/36, dev MATCH, 9 seeds MATCH)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
echo "== committed $(git rev-parse --short HEAD)"
if [ "${NOPUSH:-0}" != 1 ]; then
  git push -q origin HEAD:main
  echo "== pushed. Submit commit $(git rev-parse HEAD) on the track page (or leave SHA empty for newest on main)."
fi
