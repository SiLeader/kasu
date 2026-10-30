# Deterministic Land Simulation

English | [日本語](README_ja.md)

`land-1000-seed42.json` is the result of a release build run on 2026-10-23 on macOS arm64 / Rust 1.98.1.
It uses the production `Backend::prepare_land` / `commit_land` with real SQLite WAL / synchronous=FULL and FS / zstd.

```sh
cargo run --release --locked -p kasu-sim -- --seed 42 --clients 1000
```

1,000 clients start from the same ref and base. After every client's first prepare has completed,
commits race in an order determined by the seed.
Each request uses at most 3 attempts and backoff with deterministic jitter.
33 pre-store failures, 23 lost responses after persistence, and 27 cancellations were injected.
Events advance in virtual time and do not depend on incidental thread execution order.

| Stage | Landed | Conflict | Retry |
|---|---:|---:|---:|
| First concurrent requests | 9 | 0 | 991 |
| After resending Retries with the same key | 991 | 9 | 0 |

In the first conflict wave, every request ends within its attempt budget.
Afterwards, clients that got Retry resend with a new bounded budget and drain on a conflict-free schedule.
This keeps the test from simply returning Retry for everything and avoiding content checks.
One in every 100 requests writes different contents to the same `shared.txt`, so 9 end in explicit Conflicts.

Before reporting results, the simulator asserts the following.

- Every path / blob of all 991 accepted requests remains in the final tree.
- No duplicate landed IDs, and ref_log has 992 entries: 1 for initial ref creation plus 991 landings.
- Every final result is persisted per repo / key and matches the returned result.
- Resending all 1,000 requests from a new SQLite connection and Backend replays the original results.
- ref_log is unchanged before and after replay.

The 21 ms of virtual time is the scheduler's clock. It is not real OS time or processing time for 1,000 TCP connections.
With a small 32-client run, the same seed is run twice and the entire summary, including the result digest, is checked for equality.
Deadlines, cancellation, and dropped futures in real asynchronous processing are verified separately by the `land_runtime` test.
The nightly CI for reruns and multiple seeds is [`land-sim.yml`](../../.github/workflows/land-sim.yml).
Results of runs on GitHub are unconfirmed.

## Cache Hierarchy Load Test

`cache-1000.json` contains M9 observations over the real Objects RPC.
One authorized Blob is requested concurrently by 500 clients on each of two node daemons.
Both nodes connect to the same memory cache and authoritative server.
On each daemon 499 requests were coalesced, and the cache fetched from the server once.
A fetch from another trusted downstream was also a memory hit.
After the cache was shut down, including its existing HTTP/2 connections, 10 requests on each node were disk hits,
and a cold miss for a new Blob failed. The server's fetch count stayed at 1.

```sh
KASU_CACHE_CAPTURE="$PWD/docs/simulations/cache-1000.json" \
  cargo test -p kasu-cache --test layers thousand_clients --locked
```

This record verifies fan-in and failure paths for a single repo and one small object.
The scheduling of real TCP traffic depends on the OS. The next section adds a deterministic cache failure simulation using the same Layer.
It is not a capacity or latency guarantee for large objects or many cache instances.
A separate test limits 12 different IDs to 25 QPS with at most 2 concurrent fills,
and also checks breaking / recovery on upstream failure and stream / pending limits.

## Deterministic Cache Hierarchy Failures

`kasu-sim --scenario cache` uses the production `Layer`, Objects client / handler, two real DiskStores, and
one regional memory cache. Upstream links are HTTP/2 / gRPC over Tokio duplex.
Clients send requests to two daemon Layers, passing upstream authentication, role headers, chunks, and zstd / content hash verification.
Authoritative data is stored in a real SQLite / FS store.

The Tokio clock of a dedicated current-thread runtime is paused, and time advances only through scripted events.
While waiting for blocking worker IO to complete, the executor is kept runnable to prevent automatic time advancement.
Pacing, fetch deadlines, and the circuit breaker's cooldown / probe run on the same clock.
A bug where the circuit breaker used the OS clock was reproduced with a regression test in which cooldown never released under virtual time alone, and fixed.

```sh
cargo run --release --locked -p kasu-sim -- \
  --scenario cache --seed 42 --clients 1000
```

The seed determines the payload and the order in which clients are submitted. Inputs 0 / 1 are distinguished, and the result digest includes the seed, request count,
submission order, every client's completion result, and the event sequence and input IDs.
Running with only the old arguments returns the same result as `--scenario land`.

On 2026-10-26, seeds 0 / 1 / 42 / 2026 were each run twice on macOS arm64 / Rust 1.98.1 and
Linux arm64 with pinned Rust 1.90. All 16 runs of the 1,000-client scenario matched byte for byte for the same seed.
Initially, 499 requests on each node were coalesced, and the regional cache combined 2 requests into 1 authoritative fetch.

| Phase | Virtual time ms | Cumulative authoritative RPCs |
|---|---:|---:|
| Aggregating 1,000 requests | 5 | 1 |
| Cached hit and cold miss both fail closed while cache unreachable | 9 | 1 |
| Restart with an empty cache | 18 | 2 |
| Reject an incomplete response | 20 | 3 |
| Upstream RPC deadline expires | 80 | 5 |
| Break 30 requests after store failures | 110 | 9 |
| Probe also fails to store; break again | 140 | 10 |
| Store recovers; every tier recovers | 170 | 11 |
| Warm hit after reopening the daemon store | 170 | 11 |

The archived traces below predate authoritative identity checks on every cached read; the current simulation rejects both cached and cold reads while the identity upstream is unavailable.
A response cut midway is made to fail after a 64 KiB chunk produced by the real handler. Incomplete frames are not stored.
A stalled fetch ends at the 50 ms virtual upstream deadline and recovers on a later refetch.
After 3 store failures, 30 requests end without additional authoritative fetches, while warm node hits still work.
If the single probe after cooldown also fails to store, the breaker trips again; after recovery it returns to fetch, store, and cache hits.
Additional authoritative fetches while the cache is unreachable and when reopening the daemon DiskStore are 0, and pending is 0 at the end of every phase.

170 ms is virtual clock time, not real time or a performance figure.
It does not claim coverage of OS packet loss / real process SIGKILL, physical ENOSPC / power loss, or every fault ordering.
It is a test that injects RPC unavailability, incomplete streams, stalled responses, and Store::put errors.
The existing 6 real-TCP hierarchy tests also pass on both OSes.
All 50 cache / daemon / CLI / sim tests on macOS also pass, confirming no regressions in real processes or the Land runtime.

Results are stored in [summary.json](cache-deterministic/summary.json), [all events for seed 42](cache-deterministic/seed-42.json),
the [reproduction before the clock fix](cache-deterministic/clock-before.log), [unit](cache-deterministic/unit.log),
[real TCP](cache-deterministic/layers.log), and the [Linux build](cache-deterministic/linux-build.log).
`tools/cache-sim/record.py` checks agreement across 4 seeds × 2 runs × 2 environments, plus each phase's time, authoritative fetch count, pending, and source hashes.

```sh
docker build -f tools/cache-sim/Dockerfile -t kasu:cache-sim .
mkdir -p /tmp/kasu-cache-sim
docker run --rm --network none --user "$(id -u):$(id -g)" \
  -e KASU_SIM_OUTPUT=/reports \
  --mount type=bind,source=/tmp/kasu-cache-sim,target=/reports \
  kasu:cache-sim
```

A separate cache job has been added to the nightly `land-sim.yml`. Results of runs on GitHub are unconfirmed.

## Shared Storage Acceptance Test

`storage-host.json` / `storage-linux.json` are the results of runs on 2026-10-23.
`storage-linux-tests.log` stores the output of all Linux storage tests and the two-server tests.
PostgreSQL 18 / SeaweedFS 4.45 are pinned by digest in `tools/storage/compose.yml`.
The host is macOS arm64, and Linux is an arm64 container with pinned Rust 1.90.

```sh
python3 tools/storage/run.py --workdir /tmp/kasu-storage-host
python3 tools/storage/run.py --linux --workdir /tmp/kasu-storage-linux
```

This covers mixed CAS / conditional PUT over 16 connections, the same Land over 8 connections, SQL failures and feed ordering,
webhook resends, and TLS / authentication rejection. With real ingest and two server processes, it verifies
Git atomic push / strict fsck, sharing of Metadata / Objects, a single CAS winner,
and Land replay on another server and after restart.
Times are real time for the acceptance test including builds and environment startup, not a processing performance benchmark.
For verification scope with a local S3-compatible service and operational procedures, see [PostgreSQL / S3](../production-storage.md).

## Authoritative Server Admission and Metrics

[server-runtime/summary.json](server-runtime/summary.json) is the verification record for shared HTTP / gRPC IP / PAT quotas,
bounded queues, disconnects of blocking workers and Objects / Git packs, independent bounded scrape slots, and CAS result and Derived Data metrics.
It uses all-feature workspace tests and clippy on macOS arm64 and the Git compatibility container on Linux arm64 with pinned Rust 1.90.
Each log and the source SHA-256 are recorded. Real TCP scheduling depends on the OS,
and bucket refill / reclamation is verified separately by unit tests with a deterministic clock.
This is not a guarantee of a distributed rate limiter or production capacity. The configuration and metrics contract is in [server-runtime.md](../server-runtime.md).

## OpenTelemetry Real-process Test

[telemetry/summary.json](telemetry/summary.json) is verification with five real binaries and an OTLP HTTP receiver.
The official protobuf decoder reads stored requests and checks every parent-child relationship of a cold checkout CLI → daemon → cache → authority,
warm hits, external HTTP parents and authentication failures, sampling / disabling, absence of secrets and baggage,
and bounded termination even with collector delays / redirects / outages. It is run on macOS and Linux with pinned Rust 1.90,
storing hashes of the source and each log. Queue saturation and scoped subscribers are verified separately in unit tests.
Configuration and guarantees are in [telemetry.md](../telemetry.md).

## Daemon Capacity Management and Eviction Recovery

[disk-lru/summary.json](disk-lru/summary.json) stores the record verifying byte / index-count limits, the durable LRU, shared Tree / Blob frames,
concurrent open and snapshots, not storing a single oversized frame, and snapshot capacity pre-rejection, pinning, and generation counts.
It includes a test that observes a file / SQLite mismatch after SIGSTOP and then SIGKILLs, and refetch after eviction / warm hits after restart
through the real daemon's Unix socket. Tests using fixture child processes are run from a parent, so they are ignored in normal runs.
Logs, measurements, and source hashes for macOS / Linux with pinned Rust 1.90 are recorded. Configuration and scope are in [disk-cache.md](../disk-cache.md).
