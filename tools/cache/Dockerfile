# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f AS test
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler git && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    rustc --version && git --version && uname -sm && \
    sha256sum crates/kasu-cache/src/lib.rs crates/kasu-cache/tests/layers.rs crates/kasu-cache/src/disk.rs crates/kasu-cache/src/disk/state.rs crates/kasu-cache/tests/disk_lru.rs crates/kasu-daemon/src/main.rs crates/kasu-daemon/tests/process.rs && \
    KASU_DISK_CAPTURE=/out RUSTC_WRAPPER= cargo test -p kasu-cache -p kasu-daemon -p kasu-cli --locked
