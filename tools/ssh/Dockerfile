# syntax=docker/dockerfile:1
FROM golang:1.27.3-bookworm@sha256:972b4110ba0f47539a975c31e52c64765e1ff19f2d74469ae4a595c6676e80a5 AS go-client
WORKDIR /client
COPY tools/git-compat/go.mod tools/git-compat/go.sum ./
RUN go mod download
COPY tools/git-compat/main.go ./
RUN CGO_ENABLED=0 go build -o /out/go-client .

FROM rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler openssh-server git python3 && rm -rf /var/lib/apt/lists/* && mkdir -p /run/sshd && passwd -d root
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
WORKDIR /app
ENV RUSTC_WRAPPER=""
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    rustc --version && git --version && ssh -V && uname -sm && \
    sha256sum crates/kasu-server/src/git_ssh.rs crates/kasu-server/src/git_ssh/pack_stream.rs crates/kasu-server/src/ssh_main.rs crates/kasu-server/src/configuration.rs crates/kasu-server/tests/git_ssh.rs crates/kasu-server/src/accounts.rs crates/kasu-server/src/read_scope.rs crates/kasu-server/tests/accounts.rs crates/kasu-proto/proto/kasu.proto crates/kasu-cli/tests/e2e.rs && \
    cargo test -p kasu-server --lib --locked && \
    mkdir -p /out && cargo test -p kasu-server --test git_ssh --test accounts --no-run --locked --message-format=json | \
    python3 -c 'import sys,json,shutil; data=[json.loads(line) for line in sys.stdin]; [sys.stderr.write(a["message"].get("rendered","")) for a in data if a.get("reason")=="compiler-message"]; paths={a["target"]["name"]:a["executable"] for a in data if a.get("reason")=="compiler-artifact" and a.get("target",{}).get("name") in ("git_ssh","accounts") and a.get("executable")}; assert len(paths)==2; shutil.copyfile(paths["git_ssh"],"/out/ssh-test"); shutil.copyfile(paths["accounts"],"/out/accounts-test")' && \
    cp target/debug/kasu-git-ssh target/debug/kasu-server /out/ && chmod +x /out/ssh-test /out/accounts-test
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo test -p kasu-cli --test e2e --no-run --locked --message-format=json | \
    python3 -c 'import sys,json,shutil; data=[json.loads(line) for line in sys.stdin]; [sys.stderr.write(a["message"].get("rendered","")) for a in data if a.get("reason")=="compiler-message"]; paths=[a["executable"] for a in data if a.get("reason")=="compiler-artifact" and a.get("target",{}).get("name")=="e2e" and a.get("executable")]; assert len(paths)==1; shutil.copyfile(paths[0],"/out/cli-e2e-test")' && \
    cp target/debug/kasu /out/kasu && chmod +x /out/cli-e2e-test
RUN mkdir -p /app/target/debug && cp /out/kasu-git-ssh /out/kasu-server /out/kasu /app/target/debug/
COPY --from=go-client /out/go-client /out/go-client
ENV KASU_GO_GIT_CLIENT=/out/go-client
ENTRYPOINT ["/out/ssh-test", "--ignored", "git_over_real_openssh", "--nocapture"]
