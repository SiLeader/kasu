# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    rustc --version && uname -sm && \
    sha256sum Cargo.lock crates/kasu-cache/src/lib.rs crates/kasu-cache/Cargo.toml crates/kasu-sim/Cargo.toml crates/kasu-sim/src/cache.rs crates/kasu-sim/src/main.rs && \
    cargo test -p kasu-cache -p kasu-sim --lib --locked && \
    cargo test -p kasu-cache --test layers --locked && \
    cargo build --release -p kasu-sim --locked && mkdir -p /out && \
    cp target/release/kasu-sim /out/kasu-sim
COPY tools/cache-sim/run.sh /out/run.sh
ENTRYPOINT ["/bin/sh", "/out/run.sh"]
