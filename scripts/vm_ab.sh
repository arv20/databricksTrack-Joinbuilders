#!/bin/sh
# Dev tool: interleaved A/B timing of binaries already built into the Docker VM
# (see vm_stash_build.sh). Usage: vm_ab.sh DATA_DIR RUNS NAME...
# Prints one line per run; pipe into scripts/ab_stats.py for medians.
set -eu
DATA=$(cd "$1" && pwd); RUNS=$2; shift 2
NAME=$(printf '%s' "$DATA" | cksum | cut -d' ' -f1)
docker run --rm -i --privileged --cpus 8 -e NCPU=8 -e SS_DEBUG=1 -e RUNS="$RUNS" -e BINS="$*" \
  -e D=/var/lib/ss-bench/data/$NAME -v /var/lib/ss-bench:/var/lib/ss-bench ss-perf sh -s <<'IN'
cat $(find $D -type f) > /dev/null
for b in $BINS; do /var/lib/ss-bench/bin/ss-$b $D /tmp/o 2>/dev/null; rm -rf /tmp/o; done
i=0
while [ $i -lt $RUNS ]; do
  i=$((i+1))
  for b in $BINS; do
    rm -rf /tmp/o
    s=$(date +%s%N); /var/lib/ss-bench/bin/ss-$b $D /tmp/o 2>/tmp/log; e=$(date +%s%N)
    printf '%s wall=%s ' "$b" $(( (e-s)/1000000 ))
    grep -E 'ms\]' /tmp/log | awk '{printf "%s=%s ", $4, $5}'; echo
  done
done
IN
