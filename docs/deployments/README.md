# Deploying kasu with Docker

English | [日本語](README_ja.md)

This guide explains how to run kasu in containers. It is organized by scenario; pick the one that matches what you are building:

| Scenario | Use it for | Storage | Servers | Guide |
|---|---|---|---|---|
| Development | Trying kasu, local development, CI fixtures | SQLite and local files in a volume | 1 (no authentication) | [Development](development.md) |
| Single node | A small team or one machine in production | SQLite and local files in a volume (or PostgreSQL / S3 on the same host) | 1 | [Single node](single-node.md) |
| High availability | Several nodes, rolling upgrades, no single point of failure in kasu itself | PostgreSQL and S3-compatible storage, shared by every server | 2 or more behind a load balancer | [High availability](high-availability.md) |

The guides build on each other. Read this page first; it covers what every scenario shares.

## Images

The `Container` workflow publishes two multi-architecture images (`linux/amd64`, `linux/arm64`); see [releases and container images](../release.md) for tags.

| Image | Contents | Ports |
|---|---|---|
| `ghcr.io/sileader/kasu/server:<version>` | `kasu-server` (built with the `production` feature, so PostgreSQL and S3 are available) and `kasu-ingest` | 50051 (gRPC), 50080 (HTTP: JSON APIs, Git Smart HTTP, `/metrics`) |
| `ghcr.io/sileader/kasu/web:<version>` | `kasu-web`, the [Web UI](../web-ui.md) | 50090 |

Both run as UID/GID 10001 on `debian:bookworm-slim`. The examples below write `0.1.0` as the version; replace it with the release you deploy and pin it instead of using `latest`.

To build the images yourself from a checkout (BuildKit is required):

```sh
docker build -f docker/Dockerfile --target server -t kasu/server:local .
docker build -f docker/Dockerfile --target web -t kasu/web:local .
```

The images do not contain `kasu-cache`, `kasu-daemon`, `kasu-git-ssh` or the `kasu` CLI. Install the CLI from the release archives (or `cargo build -p kasu-cli`); run the others from source or your own image if you need them (see [cache tier](high-availability.md#optional-cache-tier)).

### How the server image starts

The entrypoint is `kasu-server` and the default command is `/data 0.0.0.0:50051 0.0.0.0:50080`, that is `kasu-server <kasu-directory> <grpc-address> <http-address>`. Arguments after the image name replace the default command, so keep the three values if you add any. Everything else is configured with environment variables, documented in the pages linked from [Configuration reference](#configuration-reference).

`kasu-ingest` is in the same image. Override the entrypoint to run it, for example `docker run --rm --entrypoint kasu-ingest ... --create /source.git /data my-repo`.

### Data and users

- `/data` is a volume. With local storage it holds `meta.db` (SQLite) and `objects/`. With PostgreSQL **and** S3 configured, the server does not create or use it.
- A named volume is initialised with the image's ownership (UID 10001), so it works as is. A bind mount must be writable by UID 10001: `sudo chown 10001:10001 /srv/kasu/data`.
- Files you mount for configuration and credentials only need to be readable by UID 10001.

## Things every deployment must get right

**Authentication is off by default.** A server that has none of `KASU_TOKEN`, `KASU_TOKEN_FILE`, `KASU_ACCOUNTS_FILE` and `KASU_PROJECTION_ACCOUNTS_FILE` set lets anyone who can reach port 50080 or 50051 read and write every repository. That is fine for a laptop, and nothing else. Bind such a server to loopback only (`127.0.0.1:50080:50080` in a `ports:` entry). See [repository accounts](../repository-accounts.md).

**Secrets are files.** Every credential has a `*_FILE` variant, and the files are read once at startup: `KASU_TOKEN_FILE`, `KASU_ACCOUNTS_FILE` (whose `token_file` entries are paths relative to the JSON file or absolute), `KASU_BINDING_KEY_FILE`, `KASU_POSTGRES_DSN_FILE`, `KASU_WEB_SESSION_KEY_FILE`, `KASU_WEB_DELEGATE_TOKEN_FILE`, `KASU_OBJECT_TOKEN_FILE`. Prefer them to `-e KASU_TOKEN=...`, which shows up in `docker inspect`. Changing a file takes effect when the container restarts.

Tokens are opaque strings of at most 4096 bytes; generate them with `openssl rand -hex 32`. Keys (`session.key`, `binding.key`) need at least 32 bytes.

**There is no built-in TLS.** `kasu-server` and `kasu-web` speak plaintext HTTP and gRPC. Terminate TLS in front of them (the examples use [Caddy](https://caddyserver.com/) and nginx) and publish only the proxy's ports. Clients reach the gRPC port with an `https://` `KASU_SERVER` URL (the CLI and the Web UI verify certificates against the WebPKI roots), so the certificate must come from a public CA or you must put your CA in the client's trust path. The PostgreSQL connection is TLS by default; see [PostgreSQL and S3](../production-storage.md).

**The server sees the proxy's address, not the client's.** Admission control keys on the socket peer address and ignores `Forwarded` / `X-Forwarded-For`, so everything behind a reverse proxy shares one per-IP quota. Raise `client_rate_per_second`, `client_burst` and `per_identity_active` with `KASU_SERVER_LIMITS_FILE` accordingly ([server limits](../server-runtime.md)). The Web UI is the exception: it forwards the browser's address in a header the server only honors from a verified delegate.

**Delegates are pinned to a network.** The server accepts the Web UI's delegate token only from `trusted_peers` (CIDR). In Docker that is the subnet of the network the two containers share, so the examples give the Compose network a fixed subnet. If a load balancer sits between `kasu-web` and the servers, the servers see the load balancer's address instead.

**Health checks.** The images contain `bash` but not `curl`. `kasu-web` serves `GET /-/healthz` and `GET /-/readyz` (the latter checks that `kasu-server` is reachable). `kasu-server` has no unauthenticated health endpoint; its `GET /metrics` needs an operator PAT, so the examples probe the TCP port. Both probes are written with bash's `/dev/tcp`:

```sh
# kasu-server: the HTTP port accepts connections
bash -c 'exec 3<>/dev/tcp/127.0.0.1/50080'
# kasu-web: ready (the server is reachable)
bash -c 'exec 3<>/dev/tcp/127.0.0.1/50090 && printf "GET /-/readyz HTTP/1.0\r\n\r\n" >&3 && head -n1 <&3 | grep -q " 200 "'
```

**Logs and traces** go to stderr (`docker logs`), filtered by `RUST_LOG`. Set `OTEL_TRACES_EXPORTER=otlp` and `OTEL_EXPORTER_OTLP_ENDPOINT` to export spans ([tracing](../telemetry.md)). Metrics are Prometheus text at `GET /metrics` on the server (operator PAT required) and `GET /-/metrics` on the Web UI (`KASU_WEB_METRICS_TOKEN_FILE`).

**Graceful shutdown.** `kasu-server` and `kasu-web` handle SIGINT only, and `docker stop` sends SIGTERM by default, which would kill them after the grace period. Set `stop_signal: SIGINT` (Compose) or `--stop-signal SIGINT` (`docker run`) so background tasks such as webhook delivery shut down cleanly. All examples do this.

## Operating the deployment

- **Upgrade** by changing the image tag and recreating the containers. Take a backup first. When the new version raises the PostgreSQL schema version, older servers refuse the new schema, so stop every old server before starting the first new one; see [PostgreSQL storage contract](../production-storage.md#postgresql-storage-contract). A rolling upgrade is therefore only safe between versions that share a schema version; check the release notes.
- **Back up** the data that is authoritative: the `/data` volume for local storage, or the PostgreSQL database and the S3 bucket/prefix together for shared storage. kasu never deletes stored objects, and the database rows reference the S3 objects, so restore both from the same point in time. The Web UI is stateless.
- **Configuration changes**: `KASU_REPOSITORIES_FILE` is re-read on SIGHUP and every 30 seconds (`docker kill -s HUP <container>`). Accounts, tokens and write policy are read at startup only, so restart the server.
- **Importing repositories** is done with `kasu-ingest` against the same storage the server uses. Each guide shows the command.

## Configuration reference

| Topic | Page |
|---|---|
| Operator token, accounts, delegates | [Repository accounts](../repository-accounts.md) |
| Several repositories, `KASU_REPOSITORIES_FILE`, Admin API | [Multiple repositories](../multi-repository.md) |
| PostgreSQL and S3 variables | [PostgreSQL / S3 and multiple servers](../production-storage.md) |
| Concurrency and rate limits, metrics | [Server limits and metrics](../server-runtime.md) |
| Web UI variables and `web.json` | [Web UI](../web-ui.md) |
| Webhooks to Argo CD / Flux | [Webhooks](../webhooks.md) |
| Git push policy | [Git push](../git-push.md) |
| Importing Git history | [Git import](../git-import.md) |
| Tracing | [OpenTelemetry](../telemetry.md) |
| Cache and daemon tiers | [Cache hierarchy](../cache-hierarchy.md), [disk cache](../disk-cache.md) |
