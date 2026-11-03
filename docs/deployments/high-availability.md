# High-Availability Deployment

English | [日本語](high-availability_ja.md)

Several `kasu-server` nodes behind a load balancer, sharing one PostgreSQL database and one S3-compatible bucket. Any node can serve any request, so a node can fail or be upgraded while the others keep serving. This guide covers the containers; operating PostgreSQL, the object store and the load balancer themselves is up to you.

Read [Deploying kasu with Docker](README.md) first, and [Single node](single-node.md) for the account and credential files this guide reuses.

## What this gives you, and what it does not

- **Servers are stateless.** All state is in PostgreSQL (metadata, refs, logs, the Land idempotency ledger, webhook cursors) and S3 (immutable objects). A server's `/data` directory is neither created nor used when both are configured ([storage contract](../production-storage.md)).
- **Writers cannot conflict.** Ref updates are compare-and-swap transactions in PostgreSQL, so two servers racing to update a branch have exactly one winner. [Land](../land.md) results are shared, so a Land whose response was lost can be re-sent with the same key to any node.
- **Availability is the minimum of its parts.** kasu does not replicate PostgreSQL or S3. Use a highly available PostgreSQL (a managed service, Patroni, or similar) and a durable S3. If the database is down, every server is unavailable.
- **Verification scope.** The shared-storage tests run two server processes and the real `kasu-ingest` against PostgreSQL 18 and SeaweedFS ([production storage](../production-storage.md#verification-against-real-services)). They do not cover real AWS IAM/KMS or network failures, geographically distributed deployments, or capacity. Load-test and rehearse failures in your environment.
- **Per-process limits.** Admission control and rate limits are enforced in each server process, not across the cluster. With N servers the effective limit is roughly N times the configured one ([server limits](../server-runtime.md)).
- **One database connection per server process.** The PostgreSQL store reuses a single synchronous connection and serializes within the process; it reconnects after a failure but never retries a failed statement by itself ([PostgreSQL storage contract](../production-storage.md#postgresql-storage-contract)). Scale by adding servers. Size `max_connections` for the number of servers plus your own tooling.
- **Webhooks are at-least-once.** Every server runs the webhook dispatcher, with cursors in PostgreSQL. After a failover a delivery can be repeated with an identical body, so the receiver must tolerate duplicates (Argo CD and Flux webhooks only trigger a refresh).

## Topology

```text
                    ┌──────────────── load balancer pair (TLS, VIP or cloud LB) ─────────────────┐
 Git / JSON ── 443 ─┤ git.example.com:443    → node*:50080                                       │
 kasu CLI ── 50051 ─┤ git.example.com:50051  → node*:50051   (HTTP/2, gRPC)                      │
 browsers ─── 443 ──┤ kasu.example.com:443   → node*:50090                                       │
                    └────────────────────────────────────────────────────────────────────────────┘
                         │                         │                         │
                 ┌───────┴───────┐         ┌───────┴───────┐         ┌───────┴───────┐
                 │ node1         │         │ node2         │         │ node3         │
                 │ server + web  │         │ server + web  │         │ server + web  │
                 └───────┬───────┘         └───────┬───────┘         └───────┬───────┘
                         └──────────────┬──────────┴──────────────┬──────────┘
                                        ▼                         ▼
                        PostgreSQL (HA, TLS)          S3 bucket (conditional writes)
```

Each node runs one `server` and one `web` container, and the Web UI talks to the server on its own node. If that server fails, the node's Web UI fails its readiness check and the load balancer stops using it. Two nodes are the minimum for availability; three tolerate a failure during maintenance of another.

## Prerequisites

| Component | Requirement |
|---|---|
| PostgreSQL | Reachable from every node over TLS. A dedicated database and role that may create the `kasu` schema. Tested with 18 |
| S3 | A bucket created beforehand. The service must support [conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) (`If-None-Match: *`) and read-after-write consistency, because kasu refuses to fall back to unconditional overwrites. Credentials allowing `GetObject` and `PutObject` on the bucket and prefix |
| Load balancer | Terminates TLS; forwards HTTP/1.1 to port 50080 and **HTTP/2 (gRPC)** to 50051; supports long-lived streaming responses (Git packs, Objects) |
| Hosts | Docker with the Compose plugin on every node; synchronized clocks; a way to distribute identical configuration and secret files to every node |
| Network | Nodes reach PostgreSQL and S3; the load balancer reaches ports 50051, 50080 and 50090 on the nodes; nothing else is exposed |

## What must be identical on every node

Servers do not coordinate with each other beyond the shared stores, so each of these must hold the same value on every node, because a mismatch shows up as users being rejected on one node only:

| Item | Why |
|---|---|
| PostgreSQL database, S3 bucket and `KASU_S3_PREFIX` | They are the shared state |
| Operator token, `accounts.json` and the PAT files it references | Credentials are checked locally against the file at startup |
| `KASU_BINDING_KEY_FILE` | Web sessions are bound to the PAT through this key; with a different key per node, a session works on only one node |
| `KASU_WEB_SESSION_KEY_FILE` (Web) | Session cookies and CSRF tokens are verified by any replica |
| `KASU_WRITE_POLICY_FILE`, `KASU_SERVER_LIMITS_FILE`, `KASU_REPOSITORIES_FILE` | Policy and quotas |
| `KASU_OBJECT_TOKEN_FILE`, `KASU_OBJECT_REPOSITORIES` | Only when you use the [cache tier](#optional-cache-tier) |

Credential and configuration files are read at startup only. To change them, update the files on every node and restart the servers one at a time. `KASU_REPOSITORIES_FILE` is the exception: it is re-read on SIGHUP and periodically, and the registry lives in PostgreSQL, so two nodes applying the same file converge.

## Node configuration

Every node runs the same Compose project. Only `NODE_IP` (the address the load balancer reaches the node on) differs. Use the file layout from [Single node](single-node.md#layout), with these changes:

- add `postgres-dsn` and `pg-ca.pem` to `secrets/`;
- put the S3 credentials in `s3.env` (mode 0600, owned by root), or use an instance role;
- the Docker subnet is the same on every node, so `accounts.json` can use the same `trusted_peers` everywhere.

`secrets/postgres-dsn` (one line; at most 64 KiB; the password is part of it, so protect the file):

```text
host=pg.internal.example.com port=5432 dbname=kasu user=kasu password=CHANGE-ME sslmode=require
```

TLS to PostgreSQL is verified against the WebPKI roots and the host name. For a private CA, mount its certificate and set `KASU_POSTGRES_CA_FILE`. `sslmode=prefer` is upgraded to `require`; plaintext needs two explicit opt-ins and is only for [local rehearsal](#rehearsal-on-one-machine).

`s3.env`:

```text
AWS_ACCESS_KEY_ID=...
AWS_SECRET_ACCESS_KEY=...
```

Credentials otherwise come from the standard AWS providers: session tokens, web identity, and the ECS/EC2 role providers. On EC2 with IMDSv2, a container needs a metadata hop limit of 2 to reach the instance role. Grant only `s3:GetObject` and `s3:PutObject` on `arn:aws:s3:::BUCKET/PREFIX/*`; kasu never deletes objects, and you should not give it that right. For S3-compatible services set `KASU_S3_ENDPOINT` and, if needed, `KASU_S3_PATH_STYLE=true`.

`compose.yaml` (identical on all nodes; start it with `NODE_IP=10.0.1.11 docker compose up -d` and so on):

```yaml
name: kasu

services:
  server:
    image: ghcr.io/sileader/kasu/server:0.1.0
    restart: unless-stopped
    stop_signal: SIGINT
    ports:
      - "${NODE_IP:?set NODE_IP to this node's private address}:50051:50051"
      - "${NODE_IP}:50080:50080"
    environment:
      KASU_POSTGRES_DSN_FILE: /run/secrets/postgres-dsn
      KASU_POSTGRES_CA_FILE: /etc/kasu/pg-ca.pem        # only for a private CA
      KASU_S3_BUCKET: kasu-prod
      KASU_S3_REGION: ap-northeast-1
      KASU_S3_PREFIX: kasu/objects/v1
      KASU_TOKEN_FILE: /run/secrets/operator-pat
      KASU_ACCOUNTS_FILE: /etc/kasu/accounts.json
      KASU_BINDING_KEY_FILE: /run/secrets/binding-key
      KASU_SERVER_LIMITS_FILE: /etc/kasu/limits.json
      KASU_PUBLIC_URL: https://git.example.com
      RUST_LOG: info
    env_file: ./secrets/s3.env
    volumes:
      - ./config/accounts.json:/etc/kasu/accounts.json:ro
      - ./config/limits.json:/etc/kasu/limits.json:ro
      - ./secrets/pg-ca.pem:/etc/kasu/pg-ca.pem:ro
    secrets: [postgres-dsn, operator-pat, alice-pat, argocd-pat, kasu-web-pat, binding-key]
    networks: [kasu]
    healthcheck:
      test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/50080"]
      interval: 10s
      timeout: 3s
      retries: 5

  web:
    image: ghcr.io/sileader/kasu/web:0.1.0
    restart: unless-stopped
    stop_signal: SIGINT
    ports:
      - "${NODE_IP}:50090:50090"
    depends_on:
      server:
        condition: service_healthy
    environment:
      KASU_WEB_SERVER: http://server:50051
      KASU_WEB_PUBLIC_URL: https://kasu.example.com
      KASU_WEB_DELEGATE_TOKEN_FILE: /run/secrets/kasu-web-pat
      KASU_WEB_SESSION_KEY_FILE: /run/secrets/web-session-key
      KASU_WEB_CONFIG_FILE: /etc/kasu-web/web.json
      RUST_LOG: info
    volumes:
      - ./config/web.json:/etc/kasu-web/web.json:ro
    secrets: [kasu-web-pat, web-session-key]
    networks:
      kasu:
        ipv4_address: 172.28.0.20
    healthcheck:
      test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/50090 && printf 'GET /-/readyz HTTP/1.0\\r\\n\\r\\n' >&3 && head -n1 <&3 | grep -q ' 200 '"]
      interval: 10s
      timeout: 3s
      retries: 5

networks:
  kasu:
    ipam:
      config:
        - subnet: 172.28.0.0/24

secrets:
  postgres-dsn: { file: ./secrets/postgres-dsn }
  operator-pat: { file: ./secrets/operator-pat }
  alice-pat: { file: ./secrets/alice-pat }
  argocd-pat: { file: ./secrets/argocd-pat }
  kasu-web-pat: { file: ./secrets/kasu-web-pat }
  binding-key: { file: ./secrets/binding-key }
  web-session-key: { file: ./secrets/web-session-key }
```

`config/accounts.json` is the one from [Single node](single-node.md#configuration-files) (the delegate's `trusted_peers` is `172.28.0.20/32` because the Web UI reaches its own node's server directly). In `config/web.json` there is no Caddy: set `trusted_forwarders` to the addresses of your load balancers. Without that, every browser appears to come from the load balancer and login rate limits are shared. The secret files and `limits.json` must be present and identical on every node; `limits.json` needs the same raised quotas as the single-node guide because all clients arrive from the load balancers' addresses.

The server and the Web UI are published on `NODE_IP` only. Do not publish them on a public interface: they have no TLS.

### Distributing configuration

Keep `config/` and `secrets/` in your configuration management (Ansible, a secret manager that renders files, or similar) and apply them to every node before changing image tags. A rolling restart is the way to apply a changed secret or `accounts.json`.

## Load balancer

Any L7 load balancer that speaks HTTP/2 to the gRPC port works. This nginx configuration (nginx 1.25.1 or later for `http2 on`) is a starting point; run it on two hosts with a floating address (keepalived/VRRP) or use your cloud's load balancer instead.

```nginx
events {}

http {
    upstream kasu_http {
        server 10.0.1.11:50080 max_fails=2 fail_timeout=5s;
        server 10.0.1.12:50080 max_fails=2 fail_timeout=5s;
        server 10.0.1.13:50080 max_fails=2 fail_timeout=5s;
        keepalive 32;
    }
    upstream kasu_grpc {
        server 10.0.1.11:50051 max_fails=2 fail_timeout=5s;
        server 10.0.1.12:50051 max_fails=2 fail_timeout=5s;
        server 10.0.1.13:50051 max_fails=2 fail_timeout=5s;
        keepalive 32;
    }
    upstream kasu_web {
        server 10.0.1.11:50090 max_fails=2 fail_timeout=5s;
        server 10.0.1.12:50090 max_fails=2 fail_timeout=5s;
        server 10.0.1.13:50090 max_fails=2 fail_timeout=5s;
        keepalive 16;
    }

    # Git Smart HTTP and the JSON APIs
    server {
        listen 443 ssl;
        http2 on;
        server_name git.example.com;
        ssl_certificate     /etc/nginx/tls/git.example.com.pem;
        ssl_certificate_key /etc/nginx/tls/git.example.com.key;

        client_max_body_size 0;
        proxy_request_buffering off;
        proxy_buffering off;
        location / {
            proxy_pass http://kasu_http;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host $host;
            proxy_connect_timeout 2s;
            proxy_read_timeout 600s;
            proxy_send_timeout 600s;
            proxy_next_upstream error timeout;   # connection failures only; never replays a started request
        }
    }

    # gRPC for the native CLI: KASU_SERVER=https://git.example.com:50051
    server {
        listen 50051 ssl;
        http2 on;
        server_name git.example.com;
        ssl_certificate     /etc/nginx/tls/git.example.com.pem;
        ssl_certificate_key /etc/nginx/tls/git.example.com.key;

        client_max_body_size 0;
        location / {
            grpc_pass grpc://kasu_grpc;
            grpc_connect_timeout 2s;
            grpc_read_timeout 600s;
            grpc_send_timeout 600s;
            grpc_next_upstream error timeout;
        }
    }

    # Web UI
    server {
        listen 443 ssl;
        http2 on;
        server_name kasu.example.com;
        ssl_certificate     /etc/nginx/tls/kasu.example.com.pem;
        ssl_certificate_key /etc/nginx/tls/kasu.example.com.key;

        location / {
            proxy_pass http://kasu_web;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host $host;
            proxy_set_header X-Forwarded-For $remote_addr;   # overwrite: the Web UI trusts the rightmost entry
            proxy_connect_timeout 2s;
        }
    }
}
```

Run nginx in a container if you like, for example `docker run -d --restart unless-stopped --network host -v /etc/nginx-kasu/nginx.conf:/etc/nginx/nginx.conf:ro -v /etc/nginx-kasu/tls:/etc/nginx/tls:ro nginx:1.27`.

Notes:

- **Health checking.** Open-source nginx only detects failures passively (`max_fails`). For active checks use HAProxy, Envoy or a cloud load balancer: check TCP on 50080/50051 for servers and `GET /-/readyz` (HTTP 200) on 50090 for the Web UI. The Web UI is ready only while its own node's server is reachable.
- **No stickiness is needed** for the server. For the Web UI, sessions are signed cookies valid on every node.
- **Retries.** Only connection-level failures are retried on another node. Do not configure the load balancer to replay requests that have already been sent: writes are not safe to repeat blindly. Clients repeat them correctly with their own idempotency (below).
- **Idle timeouts** on the load balancer must exceed the longest Git pack or Objects transfer you expect.
- **Metrics** (`/metrics`) are per process. Scrape every node directly with the operator PAT (port 50080 on the node's address), not through the load balancer.
- **Addresses.** All clients appear to the servers as the load balancers' addresses, so the per-IP quotas are shared by all users. Tune `limits.json` ([server limits](../server-runtime.md)) after load testing.

## First start and importing repositories

1. Start the first node alone with `docker compose up -d`. Startup initializes the `kasu` schema in PostgreSQL under an advisory lock, so starting all nodes together is also safe, but a single node makes errors in the shared settings easier to read.
2. Check `docker compose logs server`. kasu never falls back to local storage when PostgreSQL or S3 is configured, so a wrong DSN or bucket makes startup fail with an error.
3. Start the other nodes.
4. Create or import repositories. With shared storage you can do this while servers are running, from any node or an admin host that has the same variables:

```sh
docker compose run --rm --no-deps \
  -v /path/to/infra.git:/source.git:ro \
  --entrypoint kasu-ingest server \
  --create /source.git /data team-a/infra
```

`kasu-ingest` uses the same `KASU_POSTGRES_*` and `KASU_S3_*` variables as the server (the `server` service supplies them), and writes to the shared stores; the `/data` argument is required by the command line but unused. The repository is visible on every node immediately. See [Git import](../git-import.md).

Verify by reading through the load balancer and through each node directly:

```sh
export KASU_TOKEN="$(sudo cat secrets/operator-pat)"
for h in https://git.example.com http://10.0.1.11:50080 http://10.0.1.12:50080; do
  git -c http.extraHeader="Authorization: Bearer $KASU_TOKEN" ls-remote "$h/team-a/infra.git" | head -1
done
```

## Rolling upgrades

Plan an upgrade as one of two kinds, depending on whether the new release changes the PostgreSQL schema version. The release notes say so. When it does, servers on the old version refuse the new schema, so you cannot mix versions.

**Compatible release (same schema version).** One node at a time:

1. Take the node out of the load balancer (or let health checks do it by stopping its Web UI first).
2. Change the tags in its `compose.yaml`, then `docker compose pull && docker compose up -d`.
3. Wait until `docker compose ps` reports `healthy`, then put it back.
4. Repeat for the next node. Old and new servers run side by side meanwhile.

**Release that changes the schema.** Plan a short outage: stop every server and Web UI, take a PostgreSQL backup, start one node on the new version (it upgrades the schema under a lock and never drops anything), confirm it, then start the others. Do not leave an old server running; it would reject the new schema.

Whichever kind it is, back up PostgreSQL and S3 first.

## What happens when something fails

| Failure | Behaviour | What clients do |
|---|---|---|
| A server container or node dies | The load balancer routes around it. Requests in flight on that node fail | Retry. Reads are safe. For a native write, check the ref (compare-and-swap) before retrying; for [Land](../land.md), re-send with the same idempotency key; for `git push`, run it again |
| PostgreSQL fails over | Requests fail while the primary is unavailable. Each server reconnects on its next request; failed statements are not retried internally | Same as above |
| S3 is unavailable | Requests needing object data fail (15 s overall deadline per operation, at most 2 internal retries). No local fallback is ever used | Retry later |
| A request is cancelled mid-flight | Refs and logs are updated in one transaction that rolls back; unreferenced immutable objects may remain in S3 | Retry |
| A Web UI node dies | Other nodes accept the same session cookies | Reload |
| A load balancer node dies | The floating address moves to its partner (keepalived/VRRP or cloud LB) | Reconnect |

## Backup and recovery

- **PostgreSQL**: use your platform's continuous archiving or snapshots (point-in-time recovery). It holds refs and every index.
- **S3**: enable versioning or replication per your requirements. kasu itself never deletes objects, so keep any lifecycle rules away from the kasu prefix.
- **Consistency**: S3 objects are written before the refs that point to them. A database restored to time *T* needs the bucket as of at least *T*; extra newer objects in the bucket are harmless. Never restore the bucket to an earlier point than the database.
- **Config and secrets**: keep them in your configuration repository or secret manager.

Rehearse a full restore into a scratch environment at least once.

## Optional cache tier

For clients in other regions or large CI fleets, [`kasu-cache`](../cache-hierarchy.md) (a regional memory LRU) and `kasu-daemon` (a per-node disk cache with a Unix socket) serve the read-only object fetches close to the client. Neither is in the published images; build them from source (`cargo build --release -p kasu-cache -p kasu-daemon`) or package them in your own image. The daemon is meant to run directly on the machines that check out, since the CLI reaches it over a Unix socket.

If you introduce the tier, give every server the same `KASU_OBJECT_TOKEN_FILE`. A server with that variable rejects Objects requests that carry normal user PATs, so every client that fetches objects must then go through a daemon or cache that holds the hop token. Roll it out deliberately; it changes the client path, not just the server.

## Rehearsal on one machine

To try the topology without PostgreSQL or S3 accounts, run two servers on one host with local PostgreSQL and SeaweedFS (S3-compatible). This is the same pair of pinned images the repository's [storage tests](../production-storage.md#verification-against-real-services) use. It is a rehearsal only: it uses plaintext connections, fixed public test credentials and a well-known token, and is not a template for production.

`compose.yaml`:

```yaml
name: kasu-ha

x-server: &server
  image: ghcr.io/sileader/kasu/server:0.1.0
  stop_signal: SIGINT
  depends_on:
    postgres: { condition: service_healthy }
  environment:
    KASU_TOKEN: dev-token
    KASU_POSTGRES_DSN_FILE: /etc/kasu/postgres-dsn
    KASU_POSTGRES_ALLOW_PLAINTEXT: "true"     # with sslmode=disable in the DSN
    KASU_S3_BUCKET: kasu-rehearsal
    KASU_S3_REGION: us-east-1
    KASU_S3_ENDPOINT: http://s3:8333
    KASU_S3_PATH_STYLE: "true"
    KASU_S3_ALLOW_PLAINTEXT: "true"
    AWS_ACCESS_KEY_ID: kasu-test
    AWS_SECRET_ACCESS_KEY: kasu-local-test-secret
  configs:
    - source: postgres-dsn
      target: /etc/kasu/postgres-dsn

services:
  postgres:
    image: postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280
    environment:
      POSTGRES_USER: kasu
      POSTGRES_PASSWORD: kasu-local-test
      POSTGRES_DB: kasu
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U kasu -d kasu"]
      interval: 1s
      timeout: 3s
      retries: 30

  s3:
    image: chrislusf/seaweedfs@sha256:fc9f76fa993ad69966ffeb2f65d0318fcae39c6f8e20cf68ef7b3a5cb97769e5
    command: ["server", "-s3", "-ip=127.0.0.1", "-ip.bind=0.0.0.0", "-master.volumeSizeLimitMB=128", "-master.telemetry=false", "-volume.max=16", "-s3.config=/config/s3.json"]
    ports:
      - "127.0.0.1:58333:8333"
    configs:
      - source: s3-identities
        target: /config/s3.json

  server1:
    <<: *server
    ports: ["127.0.0.1:50051:50051", "127.0.0.1:50080:50080"]
  server2:
    <<: *server
    ports: ["127.0.0.1:51051:50051", "127.0.0.1:51080:50080"]

  lb:
    image: nginx:1.27
    depends_on: [server1, server2]
    ports:
      - "127.0.0.1:8080:8080"     # HTTP/Git through the balancer
      - "127.0.0.1:8051:8051"     # gRPC through the balancer
    configs:
      - source: nginx
        target: /etc/nginx/nginx.conf

configs:
  postgres-dsn:
    content: "host=postgres port=5432 dbname=kasu user=kasu password=kasu-local-test sslmode=disable"
  s3-identities:
    content: |
      {"identities":[{"name":"kasu-local-test","credentials":[{"accessKey":"kasu-test","secretKey":"kasu-local-test-secret"}],"actions":["Admin","Read","Write","List","Tagging"]}]}
  nginx:
    content: |
      events {}
      http {
        upstream http { server server1:50080 max_fails=1 fail_timeout=10s; server server2:50080 max_fails=1 fail_timeout=10s; }
        upstream grpc { server server1:50051 max_fails=1 fail_timeout=10s; server server2:50051 max_fails=1 fail_timeout=10s; }
        server {
          listen 8080;
          client_max_body_size 0;
          proxy_request_buffering off;
          proxy_buffering off;
          proxy_connect_timeout 2s;
          location / { proxy_pass http://http; proxy_http_version 1.1; proxy_set_header Connection ""; }
        }
        server {
          listen 8051;
          http2 on;
          grpc_connect_timeout 2s;
          location / { grpc_pass grpc://grpc; }
        }
      }
```

Start the stores, create the bucket the way the repository's test harness does, then start the servers:

```sh
docker compose up -d --wait postgres s3

until curl -sf --max-time 2 --aws-sigv4 'aws:amz:us-east-1:s3' \
      --user kasu-test:kasu-local-test-secret -X PUT http://127.0.0.1:58333/kasu-rehearsal; do sleep 1; done
docker compose exec -T s3 sh -c \
  'printf "fs.configure -locationPrefix=/buckets/ -volumeGrowthCount=1 -apply\n" | weed shell -master=127.0.0.1:9333 -filer=127.0.0.1:8888'

docker compose up -d
```

(The second command limits how many volumes SeaweedFS grows for the bucket, which matters with the small `-volume.max` used here.) Import a repository through either server while both run, then read it through the balancer and each server:

```sh
docker compose run --rm --no-deps \
  -v /path/to/source.git:/source.git:ro \
  --entrypoint kasu-ingest server1 \
  --create /source.git /data my-repo

for p in 8080 50080 51080; do
  git -c http.extraHeader='Authorization: Bearer dev-token' ls-remote http://127.0.0.1:$p/my-repo.git | head -1
done
```

Now exercise a failure: `docker compose stop server1`, and keep reading through port 8080 (nginx gives up on `server1` after a 2 second connect timeout, retries on `server2`, and skips `server1` for the next 10 seconds). `docker compose start server1` brings it back with no state to repair. Clean up with `docker compose down -v`.

## Other schedulers

The same images and environment variables work under Docker Swarm, Nomad or Kubernetes: run N replicas of the server image with the shared PostgreSQL and S3 settings, mount the files above as secrets, probe the TCP ports for health, and set the SIGINT stop signal. This guide only covers Compose.
