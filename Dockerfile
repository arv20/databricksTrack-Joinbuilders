# Build a statically linked binary, ship it alone on an empty image.
FROM rust:1-bookworm AS build
ARG TARGET_CPU=x86-64-v3
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN set -eux; \
    host=$(rustc -vV | sed -n 's/^host: //p'); \
    flags="-C target-feature=+crt-static"; \
    case "$host" in x86_64-*) flags="$flags -C target-cpu=$TARGET_CPU";; esac; \
    RUSTFLAGS="$flags" cargo build --release --locked --target "$host"; \
    cp "target/$host/release/shopping-sessions" /shopping-sessions; \
    /shopping-sessions --version >/dev/null 2>&1 || true

FROM scratch
COPY --from=build /shopping-sessions /shopping-sessions
USER 1000:1000
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/shopping-sessions"]
