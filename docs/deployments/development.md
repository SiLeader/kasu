# Development Deployment

English | [日本語](development_ja.md)

A throwaway kasu for trying the system, developing against it, or using it as a fixture in CI. It runs one server with local storage and **no authentication**, plus optionally the Web UI. Do not expose it beyond your machine. For anything shared, use [Single node](single-node.md).

Read [Deploying kasu with Docker](README.md) first for the images and conventions.

## Quick start with `docker run`

Import an existing Git repository and start the server. `/path/to/source.git` must be readable by UID 10001 (for example `chmod -R a+rX`).

```sh
docker volume create kasu-data

docker run --rm \
  -v kasu-data:/data \
  -v /path/to/source.git:/source.git:ro \
  --entrypoint kasu-ingest \
  ghcr.io/sileader/kasu/server:0.1.0 \
  --create /source.git /data my-repo

docker run -d --name kasu --stop-signal SIGINT \
  -p 127.0.0.1:50051:50051 -p 127.0.0.1:50080:50080 \
  -v kasu-data:/data \
  ghcr.io/sileader/kasu/server:0.1.0
```

`-p 127.0.0.1:...` keeps the unauthenticated server off your network. Now read the repository with Git:

```sh
git ls-remote http://127.0.0.1:50080/my-repo.git
git clone --depth=1 http://127.0.0.1:50080/my-repo.git
```

To require a token even locally (closer to a real deployment), add `-e KASU_TOKEN=dev-token` to the second command and send it with Git:

```sh
git -c http.extraHeader='Authorization: Bearer dev-token' clone http://127.0.0.1:50080/my-repo.git
```

Stop and remove everything:

```sh
docker rm -f kasu
docker volume rm kasu-data
```

## Compose: server and Web UI

The Web UI is a separate stateless process. Against a server without authentication it needs no delegate token (every visitor shares the server's open access), which is what makes this setup so short. Save this as `compose.yaml` in a scratch directory:

```yaml
name: kasu-dev

services:
  server:
    image: ghcr.io/sileader/kasu/server:0.1.0
    # To build from a checkout instead of pulling:
    # build: { context: /path/to/kasu, dockerfile: docker/Dockerfile, target: server }
    stop_signal: SIGINT
    ports:
      - "127.0.0.1:50051:50051"   # gRPC: native CLI
      - "127.0.0.1:50080:50080"   # HTTP: JSON APIs and Git
    volumes:
      - data:/data
    healthcheck:
      test: ["CMD", "bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/50080"]
      interval: 5s
      timeout: 3s
      retries: 10

  web:
    image: ghcr.io/sileader/kasu/web:0.1.0
    stop_signal: SIGINT
    depends_on:
      server:
        condition: service_healthy
    environment:
      KASU_WEB_SERVER: http://server:50051
      KASU_WEB_PUBLIC_URL: http://127.0.0.1:50090
      KASU_WEB_SESSION_KEY_FILE: /run/secrets/web-session-key
    secrets:
      - web-session-key
    ports:
      - "127.0.0.1:50090:50090"

  # One-shot importer: `docker compose run --rm ingest ...` (not started by `up`).
  ingest:
    image: ghcr.io/sileader/kasu/server:0.1.0
    profiles: ["tools"]
    entrypoint: ["kasu-ingest"]
    volumes:
      - data:/data

volumes:
  data:

secrets:
  web-session-key:
    file: ./web-session.key
```

Create the session key (development only, so world-readable is acceptable), import, and start:

```sh
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > web-session.key
chmod 644 web-session.key

docker compose run --rm \
  -v /path/to/source.git:/source.git:ro \
  ingest --create /source.git /data my-repo

docker compose up -d
```

The `ingest` run uses the same `data` volume as the server, so run it before the first `up` (or stop the server first with `docker compose stop server`). Add more repositories the same way, with names such as `team-a/infra` ([multiple repositories](../multi-repository.md)).

Open `http://127.0.0.1:50090/` for the Web UI, and clone over Git at `http://127.0.0.1:50080/my-repo.git`. The Web UI logs warnings that no delegate token is set and that cookies are not `Secure` because the public URL is `http://`; both are expected here.

### Using the native CLI

Install the `kasu` CLI from a [release archive](../release.md) (macOS or Linux), or build it with `cargo build -p kasu-cli --locked`. Then:

```sh
export KASU_SERVER=http://127.0.0.1:50051
export KASU_REPO=my-repo
kasu checkout main work
kasu -C work status
```

See the [native CLI guide](../native-cli.md) for editing, committing and landing.

### Resetting

```sh
docker compose down -v     # removes the data volume too
```

## Trying the production storage backends locally

The server image can use PostgreSQL and S3. The repository's own test harness, `tools/storage/compose.yml`, starts pinned PostgreSQL 18 and SeaweedFS (S3) containers on localhost ports 55432 and 58333 with public test credentials; `python3 tools/storage/run.py` drives it. To rehearse a deployment with several servers on one machine, use the compose file in [High availability](high-availability.md#rehearsal-on-one-machine).

## Troubleshooting

| Symptom | Cause |
|---|---|
| `Permission denied` writing `/data` with a bind mount | The directory is not writable by UID 10001: `sudo chown 10001:10001 <dir>` |
| `kasu-ingest` cannot open the source | Mount the repository directory itself (a bare repository, or a working tree's `.git`), readable by UID 10001 |
| The Web UI shows "upstream" errors | `KASU_WEB_SERVER` must be the server's gRPC URL (port 50051), not the HTTP port |
| `docker compose up` waits forever on `web` | The server health check failed; look at `docker compose logs server` |
| `kasu-ingest` says the repository "is not registered" | Pass `--create` to register a new name. Without it an import only works into an empty registry. With `--create`, a name that already exists is imported into that repository again |
