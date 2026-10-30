# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler git git-lfs=3.3.0-1+deb12u2 python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    RUSTC_WRAPPER= cargo build -p kasu-server --bin kasu-server -p kasu-ingest --bin kasu-ingest --locked && \
    mkdir -p /out && cp target/debug/kasu-server target/debug/kasu-ingest /out/ && \
    rustc --version && git --version && git lfs version
COPY tools/lfs/run.py /out/run.py
ENTRYPOINT ["python3", "/out/run.py"]
