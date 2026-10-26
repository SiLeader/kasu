# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler git python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tools/storage ./tools/storage
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build -p kasu-server -p kasu-ingest --features production --bins --locked && \
    cargo test -p kasu-store --features production --no-run --message-format=json --locked > /tmp/store-tests.jsonl && \
    cargo test -p kasu-server --features production --test shared_storage --no-run --message-format=json --locked > /tmp/shared-tests.jsonl && \
    python3 tools/storage/collect.py /tmp/store-tests.jsonl /tmp/shared-tests.jsonl
# Integration test binaries embed Cargo's server binary path.
RUN mkdir -p /app/target/debug && cp /out/kasu-server /app/target/debug/kasu-server
ENV KASU_PREBUILT_TESTS=1
CMD ["python3", "tools/storage/check.py"]
