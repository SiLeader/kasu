# Single-Node Deployment

English | [日本語](single-node_ja.md)

One machine running one `kasu-server`, the Web UI and a TLS reverse proxy, with authentication enabled. Data lives in a Docker volume (SQLite and local object files). This suits a small team, a lab, or the first stage of a larger rollout. It has no redundancy: the host is a single point of failure, and upgrades restart the server. If you need more, see [High availability](high-availability.md).

Read [Deploying kasu with Docker](README.md) first for the images, the lack of built-in TLS, and the other conventions this guide relies on.

```text
                    ┌─────────────── Docker network 172.28.0.0/24 ───────────────┐
 Git, JSON ── 443 ──┤               ┌─► server :50080  (Git, JSON APIs)          │
 kasu CLI ── 50051 ─┤ caddy (TLS) ──┼─► server :50051  (gRPC)                    │
 browsers ── 443 ───┤               └─► web    :50090 ── gRPC ──► server :50051  │
                    └────────────────────────────────────────────────────────────┘
                      server data volume: /data
```

The example uses three names that you must replace: `git.example.com` (Git, JSON APIs and gRPC on port 50051), `kasu.example.com` (Web UI) and the e-mail address Caddy uses for Let's Encrypt. Both DNS names must resolve to the host, and ports 80 and 443 must be reachable for certificate issuance.

## Layout

Create a directory for the deployment, for example `/srv/kasu`:

```text
/srv/kasu/
├── compose.yaml
├── Caddyfile
├── config/
│   ├── accounts.json
│   ├── limits.json
│   └── web.json
└── secrets/
    ├── operator-pat
    ├── alice-pat
    ├── argocd-pat
    ├── kasu-web-pat
    ├── binding-key
    └── web-session-key
```

## Credentials

Generate one random value per file. All of them are random strings; the PATs are what users and tools present as Bearer or Basic passwords.

```sh
cd /srv/kasu
mkdir -p config secrets
for n in operator-pat alice-pat argocd-pat kasu-web-pat binding-key web-session-key; do
  openssl rand -hex 32 > "secrets/$n"
done
sudo chown 10001:10001 secrets/*
sudo chmod 0400 secrets/*
```

The containers run as UID 10001, so that is the owner that makes the files readable there. Read a value back with `sudo cat secrets/alice-pat` when you hand it to its owner.

| File | Used for |
|---|---|
| `operator-pat` | The operator credential (`KASU_TOKEN_FILE`): every repository and the Admin API. Keep it for administration and bootstrapping |
| `alice-pat`, `argocd-pat` | Example [accounts](../repository-accounts.md): a maintainer and a read-only GitOps consumer |
| `kasu-web-pat` | The Web UI's [delegate](../repository-accounts.md#administration-profiles-and-delegates) token. Must differ from every other credential |
| `binding-key` | Lets Web UI sessions survive a server restart (`KASU_BINDING_KEY_FILE`; at least 32 bytes) |
| `web-session-key` | Signs the Web UI's session cookies and CSRF tokens (at least 32 bytes) |

The operator PAT cannot sign in to the Web UI; use an account PAT such as `alice-pat`.

## Configuration files

`config/accounts.json` defines who can do what ([repository accounts](../repository-accounts.md), [multiple repositories](../multi-repository.md#authentication-and-authorization)). `token_file` values are the paths inside the container, where Compose mounts the secrets.

```json
{
  "accounts": [
    {
      "name": "alice",
      "token_file": "/run/secrets/alice-pat",
      "display_name": "Alice",
      "email": "alice@example.com",
      "create_repositories": ["team-a/*"],
      "grants": [
        { "repositories": ["**"], "write_policy": {}, "admin": true }
      ]
    },
    {
      "name": "argocd",
      "token_file": "/run/secrets/argocd-pat",
      "grants": [
        { "repositories": ["team-a/*"] }
      ]
    }
  ],
  "delegates": [
    {
      "name": "kasu-web",
      "token_file": "/run/secrets/kasu-web-pat",
      "act_as": ["*"],
      "allow_admin": true,
      "trusted_peers": ["172.28.0.20/32"]
    }
  ]
}
```

`172.28.0.20` is the fixed address Compose gives the Web UI container below, so only that container can use the delegate token. Once accounts are configured, anonymous access is rejected.

`config/limits.json` raises the per-client quotas. Caddy is the only peer the server sees, so every user shares one client-IP bucket ([why](README.md#things-every-deployment-must-get-right)). Tune the values to your load; the defaults are in [server limits](../server-runtime.md).

```json
{
  "active": 128,
  "per_identity_active": 32,
  "client_rate_per_second": 2000,
  "client_burst": 4000
}
```

`config/web.json` configures the [Web UI](../web-ui.md). `trusted_forwarders` is Caddy's fixed address: only it may supply the client address in `X-Forwarded-For`.

```json
{
  "auth": { "pat_login": true },
  "trusted_forwarders": ["172.28.0.10/32"],
  "clone_urls": { "http": "https://git.example.com" },
  "default_email_domain": "users.example.com"
}
```

## Compose file

`compose.yaml`:

```yaml
name: kasu

services:
  server:
    image: ghcr.io/sileader/kasu/server:0.1.0
    restart: unless-stopped
    stop_signal: SIGINT
    environment:
      KASU_TOKEN_FILE: /run/secrets/operator-pat
      KASU_ACCOUNTS_FILE: /etc/kasu/accounts.json
      KASU_BINDING_KEY_FILE: /run/secrets/binding-key
      KASU_SERVER_LIMITS_FILE: /etc/kasu/limits.json
      KASU_PUBLIC_URL: https://git.example.com   # base of webhook clone_url
      RUST_LOG: info
    volumes:
      - data:/data
      - ./config/accounts.json:/etc/kasu/accounts.json:ro
      - ./config/limits.json:/etc/kasu/limits.json:ro
    secrets: [operator-pat, alice-pat, argocd-pat, kasu-web-pat, binding-key]
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

  caddy:
    image: caddy:2   # pin a version or digest in production
    restart: unless-stopped
    depends_on: [server, web]
    ports:
      - "80:80"
      - "443:443"
      - "443:443/udp"
      - "50051:50051"
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
      - caddy_config:/config
    networks:
      kasu:
        ipv4_address: 172.28.0.10

networks:
  kasu:
    ipam:
      config:
        - subnet: 172.28.0.0/24

volumes:
  data:
  caddy_data:
  caddy_config:

secrets:
  operator-pat:
    file: ./secrets/operator-pat
  alice-pat:
    file: ./secrets/alice-pat
  argocd-pat:
    file: ./secrets/argocd-pat
  kasu-web-pat:
    file: ./secrets/kasu-web-pat
  binding-key:
    file: ./secrets/binding-key
  web-session-key:
    file: ./secrets/web-session-key
```

Only Caddy publishes ports. The server's 50051 and 50080 are reachable only on the Docker network. If the `172.28.0.0/24` subnet collides with something on your host, pick another and change it in `compose.yaml`, `accounts.json` and `web.json`.

`Caddyfile`:

```text
{
	email admin@example.com
}

# Git Smart HTTP and the JSON APIs
git.example.com {
	reverse_proxy server:50080 {
		flush_interval -1
	}
}

# gRPC for the native CLI: KASU_SERVER=https://git.example.com:50051
git.example.com:50051 {
	reverse_proxy h2c://server:50051
}

# Web UI
kasu.example.com {
	reverse_proxy web:50090
}
```

`/metrics` on the server is reachable through `git.example.com` but needs the operator PAT. Block it at the proxy if you do not want it on the internet (`respond /metrics 404`).

## Start and verify

```sh
docker compose up -d
docker compose ps          # server and web should become "healthy"
docker compose logs -f server web caddy
```

Create a repository through the Admin API and read it back. The operator PAT is the Basic password or a Bearer token:

```sh
export KASU_SERVER=https://git.example.com:50051
export KASU_TOKEN="$(sudo cat secrets/operator-pat)"
kasu repo create team-a/infra
kasu repo list

git -c http.extraHeader="Authorization: Bearer $KASU_TOKEN" ls-remote https://git.example.com/team-a/infra.git
```

Then open `https://kasu.example.com/`, choose PAT login and paste `alice-pat`.

## Importing existing repositories

Run the importer against the same volume as the server. With local storage the server and importer share a SQLite database, so stop the server for the import; it stays down only for the length of the import:

```sh
docker compose stop server web
docker compose run --rm --no-deps \
  -v /path/to/infra.git:/source.git:ro \
  --entrypoint kasu-ingest server \
  --create /source.git /data team-a/infra
docker compose up -d
```

`/path/to/infra.git` must be readable by UID 10001. See [Git import](../git-import.md) for what is preserved. Afterwards, new work arrives through `git push` (subject to [write policy](../git-push.md)) or the native CLI, with no downtime.

## Declaring repositories and webhooks

To keep repository settings in a file instead of creating them by hand, mount a `repositories.json` and set `KASU_REPOSITORIES_FILE=/etc/kasu/repositories.json` on the server. It is re-read on SIGHUP and every 30 seconds:

```sh
docker compose kill -s HUP server
```

The file format, write policies and webhook endpoints for Argo CD or Flux are described in [multiple repositories](../multi-repository.md#per-repository-configuration) and [webhooks](../webhooks.md). Webhook secrets are files too, so mount them like the other secrets and refer to them by path.

## Backup

With local storage, everything is in the `data` volume (named `kasu_data` by the project name above). A SQLite database must not be copied while it is being written, so back up with the server stopped:

```sh
docker compose stop server web
docker run --rm -v kasu_data:/data:ro -v "$PWD/backups":/backup \
  debian:bookworm-slim tar czf "/backup/kasu-data-$(date +%Y%m%d-%H%M%S).tar.gz" -C /data .
docker compose start server web
```

Also keep `config/` and `secrets/`, which are not in the volume. To restore, recreate an empty `kasu_data` volume, extract the archive into it with ownership 10001, and start the stack. Rehearse a restore before you rely on it.

A backup without downtime requires shared storage: use PostgreSQL and S3 with their own backup tooling, as described in [High availability](high-availability.md#backup-and-recovery).

## Upgrading

1. Back up (above).
2. Change the tags in `compose.yaml` to the new version. Read the release notes first.
3. `docker compose pull && docker compose up -d`. The server restarts; clients retry or fail for those seconds.

Rolling back means restoring the backup together with the old tag; there is no downgrade path for stored data.

## Variations

- **Authentication behind a company SSO proxy.** The Web UI supports sign-in from a trusted reverse proxy's headers (`auth.proxy` in `web.json`; the delegate then needs `"allow_unbound": true`). See [Web UI](../web-ui.md#signing-in).
- **Public read-only browsing.** Add `"anonymous_account"` to the delegate and `auth.anonymous` to `web.json`. Anonymous visitors are always read-only.
- **PostgreSQL and S3 instead of the volume.** Set the variables from [PostgreSQL / S3](../production-storage.md) on `server` and remove the `data` volume. This is the first step towards [high availability](high-availability.md); with shared storage the server can be restarted, replaced or imported into without stopping it.
- **Git over SSH.** `kasu-git-ssh` is not in the image. See [Git SSH](../git-ssh.md) to run it from a build on the host.
- **A different proxy.** Any proxy that terminates TLS, forwards HTTP to port 50080, and speaks HTTP/2 cleartext (h2c) to port 50051 works instead of Caddy. The [high-availability guide](high-availability.md#load-balancer) shows nginx.
