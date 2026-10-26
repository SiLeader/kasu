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
    sha256sum crates/kasu-derived/src/fragments.rs crates/kasu-derived/src/history.rs crates/kasu-derived/tests/lineage.rs crates/kasu-proto/proto/kasu.proto crates/kasu-server/src/backend.rs crates/kasu-server/tests/metadata.rs crates/kasu-git/src/reachability.rs crates/kasu-git/src/upload.rs crates/kasu-git/src/refs.rs crates/kasu-git/tests/pack_plan.rs crates/kasu-server/src/admission.rs crates/kasu-server/src/observed_refs.rs crates/kasu-server/tests/runtime.rs crates/kasu-derived/src/scheduler.rs && \
    RUSTC_WRAPPER= cargo test -p kasu-store --test publication --locked && \
    RUSTC_WRAPPER= cargo test -p kasu-git --locked && \
    RUSTC_WRAPPER= cargo test -p kasu-derived --test lineage --test scheduler --locked && \
    KASU_CAPTURE_DIR=/out RUSTC_WRAPPER= cargo test -p kasu-server --test metadata --locked && \
    RUSTC_WRAPPER= cargo test -p kasu-server --lib --test projection_accounts --test runtime --locked && \
    KASU_CAPTURE_DIR=/out RUSTC_WRAPPER= cargo test -p kasu-server --test git_http --locked && \
    RUSTC_WRAPPER= cargo test -p kasu-server --test push --locked
