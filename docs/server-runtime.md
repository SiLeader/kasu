# Authoritative Server Limits and Metrics

English | [日本語](server-runtime_ja.md)

`kasu-server` applies admission control, shared within the process, to HTTP JSON, Git Smart HTTP, and Metadata / Writer / Objects gRPC.
`GET /metrics` serves Prometheus text format 0.0.4. It authenticates with a full repository PAT and rejects projection-only PATs and the Objects hop token.
On a public server without authentication, metrics are public too.

```sh
curl --fail -H "Authorization: Bearer $KASU_TOKEN" http://127.0.0.1:50080/metrics
```

## Startup Configuration

Default limits apply even when nothing is configured. To change them, set `KASU_SERVER_LIMITS_FILE` to the path of a JSON file.
Omitted fields use defaults; unknown fields, out-of-range values, files over 16 KiB, and unreadable files are startup errors.
Set `per_identity_active` at or below `active`.

```json
{
  "active": 64,
  "per_identity_active": 16,
  "pending": 1024,
  "queue_timeout_ms": 1000,
  "client_rate_per_second": 10000,
  "client_burst": 20000,
  "token_rate_per_second": 1000,
  "token_burst": 2000,
  "max_clients": 4096,
  "idle_ms": 60000
}
```

| Setting | Allowed range | Meaning |
|---|---|---|
| `active` | 1–4096 | Concurrent normal requests |
| `per_identity_active` | 1–`active` | Concurrency per authenticated principal. Per client IP on a public server |
| `pending` | 1–65536 | Maximum waiting requests |
| `per_repository_active` | 1–`active` | Concurrency for one repository (default half of `active`). The repository setting `limits.active` can narrow it further |
| `per_repository_pending` | 1–`pending` | Waiting requests for one repository (default 1/4 of `pending`) |
| `per_delegate_active` | 1–`active` | Concurrent requests of one [delegate](repository-accounts.md#administration-profiles-and-delegates) across every account it acts as (default `active`) |
| `queue_timeout_ms` | 1–30000 | Maximum time to wait for a concurrency slot |
| `client_rate_per_second`, `client_burst` | 1–1000000 each | Refill rate and capacity of the per-client-IP token bucket |
| `token_rate_per_second`, `token_burst` | 1–1000000 each | Refill rate and capacity of the per-principal token bucket |
| `max_clients` | 1–65536 | Maximum size of the client table. When full, new IPs are rejected |
| `idle_ms` | 1000–3600000 | Minimum time before an unused client can be reclaimed |

The IP quota is checked before credentials are verified. Authenticated requests consume both the IP and the principal
rate limits. Changing between Bearer and the Basic username does not change the principal for the same PAT.
Quotas are shared across HTTP / gRPC and every service, and the operator PAT, [each repository account](repository-accounts.md), each projection PAT, and the Objects hop token are separate principals.
The client is the socket's peer address, with IPv4-mapped IPv6 normalized to IPv4.
`Forwarded` / `X-Forwarded-For` are not used. Behind a reverse proxy, everything is aggregated under the proxy's IP.

Table cleanup requires, in addition to the idle period elapsing, that the bucket is completely refilled and no waiting or running holders remain.
This prevents quotas from resetting through expiry and duplicate creation of principals mid-request.
Invalid credentials never create per-token entries.

Requests without a slot wait in a bounded queue; a full queue, queue timeout, or rate excess returns HTTP 429 or gRPC
`RESOURCE_EXHAUSTED` with `Retry-After: 1`. Requests with side effects are not retried automatically.
For Writer / Land resends, use their existing CAS / idempotency contracts.

Concurrency slots are held until the response stream ends or is dropped. Even if a client drops its future,
the slot remains until an already started blocking worker finishes. Objects producers and Git pack delivery are included.
`/metrics` uses 2 dedicated slots so it can be scraped even when normal processing is full. These 2 slots are also held until the body is dropped,
and when full, it returns 429 without waiting. IP / token rate limits and authentication also apply to metrics.

Admission across multiple processes is independent. A shared cluster-wide quota or connection limit,
and reverse-proxy trust settings, are not part of this implementation. This limits request counts; it does not guarantee CPU or memory capacity.

Requests waiting for a repository slot give back the server-wide execution slot while waiting.
A busy repository never occupies `active` and stalls other repositories
([fairness](multi-repository.md#multi-tenant-operation)). Per-repository metrics use labels only for repositories
listed in `KASU_METRICS_REPOSITORIES`; everything else is aggregated into `other`.

A verified delegated request (for example from the Web UI) is admitted as the browser and the account it acts as: it consumes the client bucket of `kasu-client-addr` (not of the shared delegate address) and the bucket of that account, plus one `per_delegate_active` slot. `kasu-client-addr` is honored only on verified delegated requests.

## What Is Measured

All counters reset at process start. Only fixed transport / method / status values and registered
Derived Data name / version are used as labels; PATs, IPs, repos, paths, and Changeset IDs are never included.
Unknown URLs are aggregated as `Unknown`.

| Metric | Meaning |
|---|---|
| `kasu_server_requests_total{transport,method}` | Requests received, including those rejected by authentication or limits |
| `kasu_server_results_total{transport,method,code}` | Completed requests. HTTP results are also mapped to gRPC status names |
| `kasu_server_duration_seconds{transport,method}` | Histogram from waiting until every stream / worker holder has finished |
| `kasu_server_response_bytes_total{transport,method}` | Data bytes sent from bodies after passing admission. Excludes network ACKs and the size of rejection responses |
| `kasu_server_active`, `kasu_server_pending` | Number of normal requests in progress and waiting |
| `kasu_server_method_active{transport,method}` | Requests held per method, including the dedicated metrics slots |
| `kasu_server_metrics_active`, `kasu_server_client_entries` | Scrape slots in use and number of client entries |
| `kasu_server_git_cache_hits_total`, `kasu_server_git_cache_misses_total`, `kasu_server_git_cache_bytes` | Hits / misses and retained bytes of the bounded Git zlib cache |
| `kasu_server_ref_cas_total{operation,result}` | Results of native / git / mixed / land storage operations issued by the backend |
| `kasu_derived_cache_lookups_total{name,version}`, `kasu_derived_cache_hits_total{name,version}` | Existence checks against the durable cache by the scheduler, and hits |
| `kasu_derived_compute_success_total{name,version}`, `kasu_derived_compute_errors_total{name,version}` | Successes and failures of actual derivation and storage |
| `kasu_derived_compute_active{name,version}`, `kasu_derived_compute_seconds{name,version}` | Number running and latency histogram of actual computation and storage |

Disconnects and futures dropped while waiting are counted as `cancelled`, and body errors as `internal`.
requests and results differ for requests that have not yet completed. An HTTP 2xx Land business outcome is `ok` at the transport level.
CAS is counted per operation, not per ref in a batch. `land`'s `replayed` means a receipt was reused inside the storage API.
Replays returned by Land's preliminary receipt read do not call CAS and are not included.
`matched` also includes Lands that finalized a terminal result without moving the ref.

Derived Data lookups are not request counts; they include rechecks after dependency search and singleflight.
Compute latency excludes dependency search / computation and waiting for the flight lock, and measures that type's derivation and storage.
Errors before dependency resolution are not counted as compute errors. Instrumentation does not change storage keys, codecs, or Derived Data versions.
For the existing cache / daemon metrics, see the [cache hierarchy](cache-hierarchy.md).

For example, the derived cache hit rate and the 95th percentile of compute latency can be computed as follows.

```promql
rate(kasu_derived_cache_hits_total[5m]) / rate(kasu_derived_cache_lookups_total[5m])
histogram_quantile(0.95, sum by (name, version, le) (rate(kasu_derived_compute_seconds_bucket[5m])))
```

## Library Use and Verification Scope

The binary, `http_router(Arc<Backend>)`, and each Service's `into_server()` include the shared control.
Configure it with `Backend::with_limits`, and get the same measurements with `Backend::metrics`.
When building your own generated tonic server, apply `ServerLayer::grpc`.
`write_http_router` / `git_http_router` are lower-level routers for composition; when using them alone,
configure `ServerLayer::http` and `ConnectInfo<SocketAddr>`.
Embedded requests without a peer all use the same unknown-client bucket.
When adding new blocking workers, carry over the slot and tracing span using the shared helper in the server.

Tests verify refill and reclamation with a deterministic clock, queue size / deadlines / cancellation, blocking workers, body retention,
CAS conflicts / replay / cancel / error, and concurrent singleflight metrics and retries after failure.
Real HTTP / gRPC tests verify sharing across credentials and IPs, ignoring forwarding headers, authentication, label aggregation for unknown paths,
configuration loading in the real binary, and backpressure and recovery after disconnect for 8 MiB Objects / Git packs.
Run logs and environment / source hashes are stored in the [verification record](simulations/server-runtime/summary.json).

Tracing request spans carry the fixed method / transport and are carried over to blocking workers.
[OpenTelemetry export and trace context propagation](telemetry.md) has also been added. OIDC, per-account native ACLs,
and capacity / fairness evaluation with large-scale real traffic remain.
