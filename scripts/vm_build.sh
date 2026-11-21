#!/bin/sh
# Builds the binary for the Docker VM's native Linux (profile $1, default release)
# into /var/lib/ss-bench/bin/ss-$1 inside the VM (for perf runs). Dev tool only.
set -eu
PROFILE=${1:-release}
cd "$(dirname "$0")/.."
tar -cf - Cargo.toml Cargo.lock src | docker run --rm -i -v ss-cargo:/usr/local/cargo/registry \
  -v ss-target:/target -v /var/lib/ss-bench:/var/lib/ss-bench rust:1-bookworm sh -c "
  set -e; mkdir -p /src /var/lib/ss-bench/bin; tar -x -C /src; cd /src
  CARGO_TARGET_DIR=/target cargo build -q --profile $PROFILE 2>/dev/null
  cp /target/$PROFILE/shopping-sessions /var/lib/ss-bench/bin/ss-$PROFILE"
