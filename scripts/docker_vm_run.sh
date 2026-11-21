#!/bin/sh
# Like docker_run.sh (same scorer flags), for Docker VMs that cannot mount the
# host directory holding the data (e.g. Colima and ~/Desktop): the dataset is
# copied into the VM once, the container runs there, and the output is copied
# back. Usage: docker_vm_run.sh DATA_DIR OUTPUT_DIR
set -eu
IMAGE="${IMAGE:-shopping-sessions:latest}"
CPUS="${CPUS:-8}"
MEM="${MEM:-24g}"
PLATFORM="${PLATFORM:-linux/amd64}"
HELPER="${HELPER:-alpine:3.20}"
DATA=$(cd "$1" && pwd)
mkdir -p "$2"; OUT=$(cd "$2" && pwd)
NAME=$(printf '%s' "$DATA" | cksum | cut -d' ' -f1)
VMROOT=/var/lib/ss-bench
VDATA=$VMROOT/data/$NAME
VRUN=$VMROOT/run.$$
h() { docker run --rm -v $VMROOT:$VMROOT "$HELPER" sh -c "$1"; }
if ! h "test -f $VDATA/.complete"; then
  echo "copying $DATA into the VM..." >&2
  h "rm -rf $VDATA && mkdir -p $VDATA"
  tar -C "$DATA" -cf - . | docker run --rm -i -v $VMROOT:$VMROOT "$HELPER" sh -c "tar -C $VDATA -xf - && touch $VDATA/.complete"
fi
h "rm -rf $VRUN && mkdir -p $VRUN/out $VRUN/scratch && chmod 777 $VRUN/out $VRUN/scratch"
# warm the page cache with the dataset (the scorer's data is presumably cached; drop this for cold runs)
[ "${COLD:-0}" = 1 ] || h "cat \$(find $VDATA -type f) > /dev/null"
status=0
docker run --rm --platform "$PLATFORM" \
  --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
  --cap-drop ALL --security-opt no-new-privileges \
  --pids-limit 4096 --cpus "$CPUS" --memory "$MEM" --memory-swap "$MEM" \
  --user 1000:1000 \
  -v "$VDATA":/data:ro,z -v "$VRUN/out":/output:z -v "$VRUN/scratch":/scratch:z \
  -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch \
  -e NCPU="${CPUS%.*}" -e MEM_GB="${MEM%g}" \
  "$IMAGE" || status=$?
docker run --rm -v $VMROOT:$VMROOT "$HELPER" tar -C $VRUN/out -cf - . | tar -C "$OUT" -xf -
h "rm -rf $VRUN"
exit $status
