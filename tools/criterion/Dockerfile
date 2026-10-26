# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends git protobuf-compiler python3 && rm -rf /var/lib/apt/lists/*
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    rustc --version && git --version && uname -sm && \
    sha256sum Cargo.lock crates/kasu-ingest/Cargo.toml crates/kasu-ingest/benches/throughput.rs && \
    mkdir -p /out && cargo bench -p kasu-ingest --bench throughput --no-run --locked --message-format=json | \
    python3 -c 'import sys,json,shutil; data=[json.loads(line) for line in sys.stdin]; [sys.stderr.write(a["message"].get("rendered","")) for a in data if a.get("reason")=="compiler-message"]; paths=[a["executable"] for a in data if a.get("reason")=="compiler-artifact" and a.get("target",{}).get("name")=="throughput" and a.get("executable")]; assert len(paths)==1; shutil.copyfile(paths[0],"/out/throughput")' && \
    chmod +x /out/throughput
COPY tools/criterion/run.sh /out/run.sh
ENTRYPOINT ["/bin/sh", "/out/run.sh"]
