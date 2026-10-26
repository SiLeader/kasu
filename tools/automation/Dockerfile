# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tools/automation ./tools/automation
RUN python3 --version && rustc --version && python3 -m unittest discover -s tools/automation -v
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    RUSTC_WRAPPER= cargo test -p kasu-server --test automation --locked
