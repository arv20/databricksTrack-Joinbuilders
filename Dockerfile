# Build a statically linked, profile-guided-optimised binary and ship it alone
# on an empty image.
#
# PGO: build an instrumented binary, run it on a dataset made at build time by
# the published generator (pgo/generator) and on the edge-case fixtures, then
# rebuild using the recorded profile. PGO changes only code layout, inlining
# and branch weights, never what the program computes. Set PGO=0 to skip it.
FROM rust:1-bookworm AS build
# x86: build for Cascade Lake (AVX-512) when the build machine has AVX-512BW,
# else the AVX2 baseline. The scorers are Cascade Lake or newer; checking the
# build machine keeps the PGO training run (and the build) safe if it differs.
ARG TARGET_CPU=auto
ARG PGO=1
RUN rustup component add llvm-tools-preview
COPY pgo/generator /gen
RUN set -eux; if [ "$PGO" = 1 ]; then \
      cd /gen && cargo build --release --locked -q; \
      /gen/target/release/generate --seed 90210 --events 3000000 --clicks 750000 --output /pgo-data; \
    fi
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY fixtures ./fixtures
RUN set -eux; \
    host=$(rustc -vV | sed -n 's/^host: //p'); \
    flags="-C target-feature=+crt-static"; \
    case "$host" in x86_64-*) \
      cpu="$TARGET_CPU"; \
      if [ "$cpu" = auto ]; then \
        if grep -qw avx512bw /proc/cpuinfo && grep -qw avx512vl /proc/cpuinfo; then cpu=cascadelake; else cpu=x86-64-v3; fi; \
      fi; \
      echo "target-cpu: $cpu"; \
      flags="$flags -C target-cpu=$cpu";; \
    esac; \
    bin="target/$host/release/shopping-sessions"; \
    if [ "$PGO" = 1 ]; then \
      RUSTFLAGS="$flags -C profile-generate=/tmp/pgo" cargo build --release --locked --target "$host"; \
      for i in 1 2; do rm -rf /tmp/o; "$bin" /pgo-data /tmp/o 2>/dev/null; done; \
      for f in fixtures/*/input; do rm -rf /tmp/o; "$bin" "$f" /tmp/o 2>/dev/null; done; \
      "$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata" merge -o /tmp/pgo.profdata /tmp/pgo; \
      flags="$flags -C profile-use=/tmp/pgo.profdata"; \
    fi; \
    RUSTFLAGS="$flags" cargo build --release --locked --target "$host"; \
    cp "$bin" /shopping-sessions

FROM scratch
COPY --from=build /shopping-sessions /shopping-sessions
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/shopping-sessions"]
