# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler python3 git && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tools/telemetry ./tools/telemetry
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    rustc --version && python3 --version && git --version && uname -sm && \
    sha256sum crates/kasu-telemetry/src/*.rs tools/telemetry/verify.py && \
    RUSTC_WRAPPER= cargo test -p kasu-telemetry --locked && \
    RUSTC_WRAPPER= cargo build -p kasu-server -p kasu-cli -p kasu-cache -p kasu-daemon -p kasu-ingest --locked && \
    RUSTC_WRAPPER= cargo build -p kasu-telemetry --example inspect_otlp --locked && \
    python3 tools/telemetry/verify.py --out /out
