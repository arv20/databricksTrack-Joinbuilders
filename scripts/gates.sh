#!/bin/sh
# Correctness gates 1-3: fixtures, dev, oracle seeds. Usage: scripts/gates.sh [BINARY]
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WS=${WS:-$(cd "$ROOT/.." && pwd)}
BIN=${1:-$ROOT/target/release/shopping-sessions}
PY="${PY:-$WS/.venv/bin/python $WS/oracle/py39_shim.py}"
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT
fail=0
echo "== fixtures"
$PY "$ROOT/scripts/run_fixtures.py" "$ROOT/fixtures" -- "$BIN" | tail -1 | tee "$OUT/fx.txt"
grep -q '^36/36' "$OUT/fx.txt" || fail=1
check() { # name data answer
  "$BIN" "$2" "$OUT/$1" 2>"$OUT/$1.log" || { echo "$1: exit $?"; fail=1; return; }
  if $PY "$ROOT/scripts/compare.py" "$OUT/$1" "$3" >"$OUT/$1.cmp" 2>&1 && grep -q '^MATCH' "$OUT/$1.cmp"; then
    echo "$1: MATCH"
  else
    echo "$1: DIFFER"; cat "$OUT/$1.cmp"; fail=1
  fi
}
echo "== dev"
check dev "$WS/data/dev" "$WS/dev-answer"
echo "== seeds"
for s in 2 3 4 5 11 62 66 133; do
  check "seed$s" "$WS/data/seed$s" "$WS/answers/seed$s"
done
[ $fail -eq 0 ] && echo "ALL GATES PASSED" || echo "GATES FAILED"
exit $fail
