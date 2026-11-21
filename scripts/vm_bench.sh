#!/bin/sh
# Dev tool: build natively in the Docker VM and time N runs on a dataset that
# docker_vm_run.sh already copied in. Usage: vm_bench.sh DATA_DIR [RUNS] [PERF_ARGS...]
# Env: PROFILE (release|profiling), ENVS (extra docker -e flags).
set -eu
DATA=$(cd "$1" && pwd); RUNS=${2:-3}; shift 2 2>/dev/null || shift $#
NAME=$(printf '%s' "$DATA" | cksum | cut -d' ' -f1)
"$(dirname "$0")/vm_build.sh" ${PROFILE:-release} >/dev/null
docker run --rm -i --privileged --cpus 8 -e NCPU=8 ${ENVS:-} -e RUNS="$RUNS" -e PERFARGS="$*" \
  -e D=/var/lib/ss-bench/data/$NAME -e B=/var/lib/ss-bench/bin/ss-${PROFILE:-release} \
  -v /var/lib/ss-bench:/var/lib/ss-bench ss-perf sh -s <<'IN'
cat $(find $D -type f) > /dev/null
i=0
while [ $i -lt $RUNS ]; do
  i=$((i+1)); rm -rf /tmp/o
  s=$(date +%s%N); $B $D /tmp/o 2>/tmp/log; e=$(date +%s%N)
  grep -E 'ms\]' /tmp/log | awk '{printf "%s=%s ", $4, $5}'; echo
  grep -E 'exit at|Rss|Huge|Anon' /tmp/log || true
  echo "wall $(( (e-s)/1000000 )) ms  (end $((e/1000000)))"
done
if [ -n "$PERFARGS" ]; then rm -rf /tmp/o; /usr/lib/linux-tools/6.8.0-117-generic/perf $PERFARGS $B $D /tmp/o 2>&1 | grep -v 'ms\]'; fi
IN
