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
    rustc --version && git --version && uname -sm && \
    sha256sum crates/kasu-ingest/tests/roundtrip.rs crates/kasu-ingest/tests/fixtures/upstream-submodule-refs.txt crates/kasu-ingest/tests/fixtures/upstream-byte-path-refs.txt crates/kasu-ingest/tests/fixtures/upstream-byte-path-objects.txt crates/kasu-ingest/src/lib.rs && \
    mkdir -p /out && cargo test -p kasu-ingest --test roundtrip --no-run --locked --message-format=json | \
    python3 -c 'import sys,json,shutil; data=[json.loads(line) for line in sys.stdin]; [sys.stderr.write(a["message"].get("rendered","")) for a in data if a.get("reason")=="compiler-message"]; paths=[a["executable"] for a in data if a.get("reason")=="compiler-artifact" and a.get("target",{}).get("name")=="roundtrip" and a.get("executable")]; assert len(paths)==1; shutil.copyfile(paths[0],"/out/roundtrip-test")' && \
    cp target/debug/kasu-ingest /out/kasu-ingest && chmod +x /out/roundtrip-test
RUN mkdir -p /app/target/debug && cp /out/kasu-ingest /app/target/debug/kasu-ingest
COPY tools/oss-fixtures/run.sh /out/run.sh
ENTRYPOINT ["/bin/sh", "/out/run.sh"]
