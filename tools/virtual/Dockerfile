# syntax=docker/dockerfile:1
FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler fuse3 util-linux python3 && rm -rf /var/lib/apt/lists/*
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN rustup component add clippy
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    rustc --version && uname -sm && \
    sha256sum crates/kasu-cli/src/virtual_fs.rs crates/kasu-cli/src/virtual_fs/lower.rs crates/kasu-cli/src/remote.rs crates/kasu-cli/tests/e2e.rs crates/kasu-proto/proto/kasu.proto crates/kasu-server/src/backend.rs && \
    cargo clippy -p kasu-cli --all-targets --locked -- -D warnings && \
    mkdir -p /out && \
    cargo test -p kasu-cli --test e2e --no-run --locked --message-format=json | \
    python3 -c 'import sys,json,shutil; artifacts=[json.loads(line) for line in sys.stdin]; exe=[a["executable"] for a in artifacts if a.get("reason")=="compiler-artifact" and a.get("target",{}).get("name")=="e2e" and a.get("executable")]; assert len(exe)==1; shutil.copyfile(exe[0],"/out/virtual-test")' && \
    cp target/debug/kasu /out/kasu && chmod +x /out/virtual-test
RUN mkdir -p /app/target/debug && cp /out/kasu /app/target/debug/kasu
ENTRYPOINT ["/out/virtual-test", "--ignored", "virtual_mount", "--nocapture"]
