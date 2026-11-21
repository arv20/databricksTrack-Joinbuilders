#!/bin/sh
# Dev tool: build the current working tree inside the Docker VM and keep the
# binary as /var/lib/ss-bench/bin/ss-NAME (for vm_ab.sh). Usage: vm_stash_build.sh NAME
set -eu
cd "$(dirname "$0")/.."
"$(dirname "$0")/vm_build.sh" release >/dev/null
docker run --rm -v /var/lib/ss-bench:/var/lib/ss-bench alpine:3.20 \
  cp /var/lib/ss-bench/bin/ss-release "/var/lib/ss-bench/bin/ss-$1"
