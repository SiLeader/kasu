# Object Cache / Daemon Hierarchy

English | [日本語](cache-hierarchy_ja.md)

`kasu-cache` provides a regional memory LRU, and `kasu-daemon` a node's durable disk.
Both provide only GetTrees / GetBlobs of the [Objects RPC](object-api.md);
for Metadata / Writer, the CLI connects to the authoritative server.

```text
kasu CLI ─ Unix socket ─ daemon ─ gRPC ─ cache ─ gRPC ─ server
                         disk            memory       authority
```

Each instance is fixed to one repo and one downstream token.
The daemon's Unix socket is mode 600, connectable only by its owner.
It is shared by jobs running as the same UID on the node. The socket's parent must be a directory with no group / other permissions,
such as mode 700 owned by the same UID.

## Startup

```sh
cargo build -p kasu-server -p kasu-cache -p kasu-daemon -p kasu-cli --locked
export PATH="$(pwd)/target/debug:$PATH"
export KASU_TOKEN=dev-token
export KASU_SERVER=http://127.0.0.1:50051
export KASU_REPO=my-repo
export KASU_LAYER_RUN="$(mktemp -d)"
openssl rand -hex 32 > "$KASU_LAYER_RUN/server-hop"
openssl rand -hex 32 > "$KASU_LAYER_RUN/daemon-hop"
printf '%s' "$KASU_TOKEN" > "$KASU_LAYER_RUN/user-token"
```

Start each process in a separate terminal with the same environment variables.
Prepare the server's data with [Git import](git-import.md) or similar.

```sh
KASU_OBJECT_TOKEN_FILE="$KASU_LAYER_RUN/server-hop" \
  kasu-server /path/to/kasu-data "$KASU_REPO"

kasu-cache --repo "$KASU_REPO" --upstream "$KASU_SERVER" \
  --token-file "$KASU_LAYER_RUN/daemon-hop" \
  --upstream-token-file "$KASU_LAYER_RUN/server-hop" \
  --listen 127.0.0.1:50091 --metrics 127.0.0.1:50092

kasu-daemon serve --repo "$KASU_REPO" --upstream http://127.0.0.1:50091 \
  --token-file "$KASU_LAYER_RUN/user-token" \
  --upstream-token-file "$KASU_LAYER_RUN/daemon-hop" \
  --data "$KASU_LAYER_RUN/store" --socket "$KASU_LAYER_RUN/node.sock" \
  --metrics 127.0.0.1:50093

KASU_OBJECTS_ENDPOINT="unix://$KASU_LAYER_RUN/node.sock" \
  kasu checkout main work --sparse app
```

Each startup command above runs in the foreground and can be stopped with SIGINT.
Tokens are read from files at startup, with trailing CR / LF removed.
Files are at most 4096 bytes including newlines, and empty values are rejected.
`--token-file` / `--upstream-token-file` can also be set with `KASU_LAYER_TOKEN_FILE` /
`KASU_UPSTREAM_TOKEN_FILE`. Tokens are never stored in working copies or logs.
The cache's TCP listener is plaintext. For exposure to other hosts, provide TLS termination;
HTTPS connections to the upstream use WebPKI roots.

A server with `KASU_OBJECT_TOKEN_FILE` set rejects Objects RPCs with normal user PATs.
The cache receives the daemon token and the daemon receives the user token, and identical values for two adjacent tokens
are rejected. The upstream response's `x-kasu-object-layer` is also checked.
This configuration and credential separation prevent skipping tiers.
If the server omits this variable, the existing direct Objects connection is allowed.

The CLI's global `--objects <URL>` / `KASU_OBJECTS_ENDPOINT` is the endpoint for Objects only.
checkout stores the chosen URL, and later commands can override it for that run only.
It can also be used to migrate existing working copies. Over Unix sockets, the response's daemon role is also checked.
There is no fallback that reconnects to another upstream or the server when the daemon / cache fails.

`--repo` is an optional allow list that can be repeated. When omitted, every repository is handled.
Frames are shared across repositories, but only `(repo, id)` pairs whose membership in that repository the upstream has confirmed
are hits ([multiple repositories](multi-repository.md#clients)).

## Load Limits

Each process uses the following defaults, applied to the total of all downstream connections.

| Option | Default | Applies to |
|---|---:|---|
| `--streams` | 16 | Concurrent Objects RPCs. Excess is rejected immediately |
| `--fills` | 2 | Concurrent fills doing disk fetch, upstream fetch, verification, and storage |
| `--pending` | 128 | Distinct incomplete objects. Excess is rejected immediately |
| `--qps` | 20 | Paces upstream fetch starts at fixed intervals |
| `--queue-timeout-ms` | 1000 | Deadline for waiting for a fill slot |
| `--upstream-timeout-ms` | 5000 | Deadline for fetching one object from the upstream |
| `--cooldown-ms` | 1000 | Breaker time after 3 consecutive failures of upstream fetch or post-fetch storage |
| cache `--memory-bytes` | 67108864 | Total compressed frames kept in the LRU |

Incomplete requests for the same kind / ID are combined into one with singleflight.
Even if the first requester disconnects, the bounded fill continues for other requesters and for cache storage.
After the breaker time, only one probe is allowed, and processing resumes if it has recovered. There is no automatic retry.
When the cache stops, the daemon's existing disk hits still work, but cold misses fail.
Rate limits are per process. When adding cache instances, match their total to the server's capacity.

The LRU budget counts retained compressed bytes. Frames being delivered, RPC buffers, and decompression memory during verification
are needed in addition. Objects are verified against core's 256 MiB decompression limit and the shared compressed frame limit.
RPCs use at most 128 distinct IDs, chunks of at most 64 KiB, and bounded send queues.
Blocking IO time to storage itself has no deadline, so a slow disk occupies fill slots.

Stored zstd frames are forwarded without recompression.
Misses and disk hits decompress, hash, and do canonical Tree verification; memory hits use already verified frames.
Trees / Blobs have separate typed indexes, and unindexed data is never served.
Corrupt compressed frames are treated as errors, and are neither deleted automatically nor refetched from the upstream.
For recovery of interrupted publication / eviction and reclamation of unpublished data, see [disk capacity management](disk-cache.md).

## Snapshots and Disk Management

```sh
kasu-daemon snapshot --data "$KASU_LAYER_RUN/store" --repo "$KASU_REPO" /path/to/node.tar

kasu-daemon serve --repo "$KASU_REPO" --upstream http://127.0.0.1:50091 \
  --token-file "$KASU_LAYER_RUN/user-token" \
  --upstream-token-file "$KASU_LAYER_RUN/daemon-hop" \
  --data "$KASU_LAYER_RUN/restored" --socket "$KASU_LAYER_RUN/restored.sock" \
  --snapshot /path/to/node.tar
```

Manual export never overwrites existing files. Data in use by a running daemon can also be exported.
serve saves `data/snapshots/kasu-snapshot-v1-<timestamp>.tar` every 3600 seconds by default.
`--snapshot-interval-secs 0` disables periodic saving.
By default the daemon uses a durable LRU of 8 GiB of zstd payload and 100,000 typed index entries, and keeps 2 generations of periodic snapshots.
These can be changed with `--disk-bytes` / `--disk-entries` / `--snapshot-keep`.
For accounting of shared frames, migration and recovery at startup, data not counted toward capacity, and handling of old archives, see [disk capacity management](disk-cache.md).

The format is a tar with a leading `manifest.json` and `objects/<kind>/<id>.zst`.
Repo, type, ID, size, and hash are checked, and links, other paths, duplicate keys of the same type, and oversized manifests are rejected.
At most 100,000 objects, a manifest of at most 16 MiB, and by default 8 GiB of frames in total at startup.
The latter can be set with `--snapshot-max-bytes`. The archive itself also has an extra 64 MiB allowance.
The tar is never extracted directly to the filesystem; every entry is verified in a temporary store before the typed indexes are published.
Only part of an invalid snapshot never becomes servable.
If an IO failure occurs during publication after verification, part of the verified data may remain.
Snapshots are assumed to be distributed by trusted operators; keep authorization and the repo mapping intact. Every object request verifies the current repository identity upstream before serving cached frames. If that identity service is unavailable, the layer fails closed, including for snapshot-backed warm hits.

The repo of a disk is fixed in `scope.json`, and reuse for a different repo is rejected.
Double starts with the same data directory and socket are rejected with a lock.
After a forced kill, only a stopped socket with a matching owner is recreated, and on normal exit the daemon removes its own socket.

## Observability and Verification

`--metrics <address>` publishes Prometheus text at HTTP `/metrics`.
No metrics port is opened by default. It is an unauthenticated operations endpoint, so control where it listens.
`kasu_object_*{layer="cache|daemon"}` exposes hits / misses / coalesced / rejected / upstream / errors,
pending, retained count / bytes, and breaker trips.
active / max_active are the number of fills from the start of the upstream call until verification and storage complete.
requests counts authorized object lookups and does not include every rejected RPC.
Looking at the hit rate together with the coalesced count shows how effective singleflight is for cold bursts.

```sh
cargo test -p kasu-cache -p kasu-daemon -p kasu-cli --locked
docker build -f tools/cache/Dockerfile -t kasu:cache-test .
```

Over real TCP / Unix sockets, this verifies 1,000 clients, pacing across different IDs, fill / pending / stream limits,
circuit breaking and recovery, disconnects, rejection of corrupt / linked / oversized snapshots, restarting real binaries, and the CLI.
For the scope of the load test and observed values, see the [record](simulations/README.md#cache-hierarchy-load-test).

## Deterministic Failure Simulation

The [virtual-clock cache scenario](simulations/README.md#deterministic-cache-hierarchy-failures) uses the production Layer and Objects
to verify aggregation across 2 nodes, an unreachable cache, streams cut midway, fetch deadlines, store failures with cooldown / probe, and disk reopening.
Pacing, fetch deadlines, and the circuit breaker use the same Tokio clock: real time in the normal runtime and virtual time in the paused test runtime.
4 seeds were each run twice on macOS / Linux with pinned Rust 1.90, and every event and result digest for 1,000 clients matched.
