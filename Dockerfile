FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN set -e; mkdir -p /out/p /t; \
    (cd /src && cargo build -q --release --target x86_64-unknown-linux-musl --target-dir /t/probe && cp /t/probe/x86_64-unknown-linux-musl/release/probe /out/probe) > /t/probe.log 2>&1 & \
    (cd /src/gen && cargo build -q --release --target x86_64-unknown-linux-musl --target-dir /t/gen && cp /t/gen/x86_64-unknown-linux-musl/release/generate /out/generate) > /t/gen.log 2>&1 & \
    cd /src/pipe; \
    for v in fc:: f:free: fc256:freec256: f256:free256: zmm:zmm: ymm:ymm: fc14::14 fc16::16; do \
      n=${v%%:*}; r=${v#*:}; feat=${r%%:*}; bb=${r#*:}; \
      (if [ -n "$bb" ]; then export XL_BB=$bb; fi; \
       cargo build -q --release --target x86_64-unknown-linux-musl --target-dir /t/$n ${feat:+--features $feat} \
       && cp /t/$n/x86_64-unknown-linux-musl/release/pipeline /out/p/$n) > /t/$n.log 2>&1 & \
    done; \
    wait; \
    for b in probe generate p/fc p/f p/fc256 p/f256 p/zmm p/ymm p/fc14 p/fc16; do \
      [ -x /out/$b ] || { echo "missing $b"; cat /t/*.log; exit 1; }; \
    done; \
    ls -la /out /out/p

FROM scratch
COPY --from=build /out/ /
USER 1000:1000
ENTRYPOINT ["/probe"]
