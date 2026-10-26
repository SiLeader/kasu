# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends git python3 && rm -rf /var/lib/apt/lists/*
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    rustc --version && uname -sm && \
    sha256sum crates/kasu-ops/src/lib.rs crates/kasu-ops/tests/operations.rs crates/kasu-ops/tests/first_parent.rs crates/kasu-derived/src/types.rs crates/kasu-derived/src/scheduler.rs crates/kasu-ops/src/metadata.rs crates/kasu-ops/benches/divergent_history.rs && \
    cargo test -p kasu-derived --test derived --test scheduler --locked && \
    cargo test -p kasu-ops --locked && mkdir -p /out && \
    cargo bench -p kasu-ops --bench divergent_history --no-run --locked --message-format=json | \
    python3 -c 'import sys,json,shutil; data=[json.loads(line) for line in sys.stdin]; [sys.stderr.write(a["message"].get("rendered","")) for a in data if a.get("reason")=="compiler-message"]; paths=[a["executable"] for a in data if a.get("reason")=="compiler-artifact" and a.get("target",{}).get("name")=="divergent_history" and a.get("executable")]; assert len(paths)==1; shutil.copyfile(paths[0],"/out/history-bench")' && \
    chmod +x /out/history-bench
ENV KASU_BENCH_DIVERGENT_HISTORY=1
ENTRYPOINT ["/out/history-bench"]
