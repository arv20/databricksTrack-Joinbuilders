FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN set -e; mkdir -p /out/p /t; \
    (cd /src && cargo build -q --release --target x86_64-unknown-linux-musl --target-dir /t/probe && cp /t/probe/x86_64-unknown-linux-musl/release/probe /out/probe) > /t/probe.log 2>&1 & \
    (cd /src/gen && cargo build -q --release --target x86_64-unknown-linux-musl --target-dir /t/gen && cp /t/gen/x86_64-unknown-linux-musl/release/generate /out/generate) > /t/gen.log 2>&1 & \
    (cp -r /src/pipe /t/m_src && cd /t/m_src && cargo build -q --release --target x86_64-unknown-linux-musl --target-dir /t/m && cp /t/m/x86_64-unknown-linux-musl/release/pipeline /out/p/m) > /t/m.log 2>&1 & \
    (cp -r /src/pipe /t/al_src && cp /src/pipe_al/setup.rs /t/al_src/src/setup.rs && cd /t/al_src && cargo build -q --release --target x86_64-unknown-linux-musl --target-dir /t/al && cp /t/al/x86_64-unknown-linux-musl/release/pipeline /out/p/al) > /t/al.log 2>&1 & \
    wait; \
    for b in probe generate p/m p/al; do \
      [ -x /out/$b ] || { echo "missing $b"; cat /t/*.log; exit 1; }; \
    done; \
    ls -la /out /out/p

FROM scratch
COPY --from=build /out/ /
USER 1000:1000
ENTRYPOINT ["/probe"]
