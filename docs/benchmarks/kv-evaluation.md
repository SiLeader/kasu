# Initial Evaluation of a Distributed KV

English | [日本語](kv-evaluation_ja.md)

TiKV v8.5.8 was measured as an M10 candidate. The current production storage remains
[PostgreSQL / S3](../production-storage.md).
With TiKV, atomic multi-region updates and resends after replica failures were confirmed,
but protecting read conditions, metadata size, and handling contention on ordered logs would be needed when porting.

The evaluation code is a standalone Go module in [`tools/kv-eval`](../../tools/kv-eval/).
What is measured here are prototype implementations of storage primitives. The Rust `kasu-store`
PostgreSQL implementation and real-server verification are recorded in the [shared storage tests](../production-storage.md#verification-against-real-services).

## Rerunning

```sh
python3 tools/kv-eval/run.py --workdir /tmp/kasu-kv-evaluation
```

Go 1.27.3 and Docker were used. Docker memory is checked for at least 10 GiB, and 12 GB or more is recommended.
The script starts 3 PD, 3 TiKV, and a single PostgreSQL for comparison in a dedicated Compose project.
No ports are exposed to the host. Only public test passwords are used,
and this run's containers and volumes are deleted at exit. `--keep` keeps them for investigation.
The default for `--operations` is 256 per workload.

TiKV / PD v8.5.8 and PostgreSQL 18 in `compose.yml` are pinned by image digest.
Each TiKV is limited to 1 CPU / 3 GiB, with a 128 MiB block cache and 64 MiB Raft Engine memory setting.
Each PD has 512 MiB and PostgreSQL a 1 GiB limit.
Everything runs on the same Docker host, so failures and network latency across machines or AZs are not included.
TiKV initially OOMed at startup with a 1.5 GiB limit and was adjusted to the settings above.

The client uses `client-go/v2 v2.0.8-0.20260803075849-c3b50791b9fb` from
[TiDB v8.5.8's go.mod](https://raw.githubusercontent.com/pingcap/tidb/v8.5.8/go.mod), with dependencies such as gRPC / etcd / x/net
pinned to audited fixed versions. `go vet` and `govulncheck v1.9.0` also run every time.
The Go version used, SHA-256 of the binary and all evaluation sources, configuration, and JSON / logs for each stage are left in the working directory.

## What Is Stored and Measurement Conditions

TiKV uses the client's default TxnKV API v1, fixed to optimistic transactions and normal 2PC.
1PC / async commit are disabled. Following the [client's API selection guidance](https://docs.rs/tikv-client/0.4.0/tikv_client/#choosing-an-api),
this evaluation does not mix in RawKV. API v2 / keyspace isolation and separate RawKV clusters are unevaluated.

The immutable KV transactionally puts-if-absent an envelope containing a BLAKE3 index and the original key and value.
Refs have a target and monotonic version, logs a per-repo / per-stream counter,
and idempotent results a repo / key and fingerprint. Updates, both kinds of logs, and results are stored in the same transaction.
The record format in this prototype is a JSON / binary envelope.
The PostgreSQL comparison implementation also stores the same keys and values in a `SERIALIZABLE` KV table.
The production PostgreSQL uses a dedicated schema and explicit row locks.

Regions are explicitly split, and it is checked that refs / logs / counters / results land in different regions.
The 8-repo workload places counters in 8 regions with multiple leaders,
and waits until each region has 3 peers and scatter operators complete.
Routes / connections are warmed up before measuring. This placement also incurs cross-region 2PC cost.
Optimizing by placing all of a repo's records in the same region is unevaluated.

Each request has at most 4 transaction attempts and a default 3-second deadline.
`Retry` is kept in the first-response statistics, and is then resent with the same key, fingerprint, and expected under a new bounded budget.
All refs / logs after resends are checked, rejecting missing successful / resent results and double landings.
Cancellation, pre-store rollback, lost responses after storing, native / Git tombstones and ABA,
long non-UTF-8 keys and empty values, and key reuse with a different fingerprint are also checked.

This workload compares metadata KV. Blobs continue to be stored in S3.
Single-node PostgreSQL and 3-replica TiKV differ in configuration and fault tolerance,
so the measurements cannot be treated as a comparison of equivalent HA configurations or as a performance guarantee for the Rust server.

## Storing Results with Read Conditions

Assuming a checked Land result is stored without moving the ref, the following ordering was constructed.

1. A reads ref v1 and holds a transaction that writes a result under a different key.
2. B commits ref v2.
3. C reads ref v2 and "no result from A" in one snapshot.
4. A attempts to commit the result.

The comparison PostgreSQL `SERIALIZABLE` rejected A as a conflict.
Under TiKV's snapshot isolation, A's read-only dependency alone could not stop the commit.
Adding a fence where A writes back the same ref value in TiKV detected the conflict, and the stale result's commit was rejected.
This shows that porting to TiKV requires explicitly protecting read conditions, for example by including the checked keys in the write set.
The production PostgreSQL `commit_land` holds the ref row lock until the result is stored.

## Size and Failure Tests

1 KiB / 1 MiB immutable values could be stored and fetched in both.
An 8 MiB raw value could be stored in the comparison PostgreSQL, while this TiKV configuration
rejected it as exceeding the Raft entry size including the envelope. It is also checked that rejected values are not published.
These are 3-point probes, so the full maximum allowed size has not been determined.
Storing large Changesets and the like requires evaluating configuration changes or indirect references to S3.

Failures are injected into regions verified to have 3 peers.

- `SIGKILL` the data leader and the PD leader separately. After leader replacement, check that updates, lost responses, and resends result in a single publication.
- Stop two nodes including the data leader. Check that no write success is returned within the 2-second transaction deadline.
- After quorum recovers, resend the same request with a bounded budget of at most 10. Record each `Retry` and elapsed time, and land without changing the original expected.
- Restart every process. Check digests and counts of every stable record, such as immutable values, refs, logs, and results,
  and separately check in full the refs / counters / log bodies / 4 idempotent results that change during failures.

Leader replacement time includes Docker operations and observation polling, and is recorded separately from request time after replacement.
Being able to connect over TCP right after recovery does not necessarily mean writes succeed immediately.
This does not cover every forced-kill timing of in-progress commits or every interleaving as Jepsen would.

## Observations on 2026-10-23

Raw data is stored in the [run summary](kv-evaluation/summary.json), [PostgreSQL](kv-evaluation/postgres-baseline.json), and [TiKV](kv-evaluation/tikv-baseline.json).
Same Linux arm64 host, 256 operations per workload, 4 KiB immutable values.

| Prototype | Operation | Concurrency | First-try success | Retry | Response p95 ms | First-try Ack/s | Resend drain ms |
|---|---|---:|---:|---:|---:|---:|---:|
| postgres | Immutable PUT | 1 | 256 | 0 | 0.276 | 4551.3 | 0.0 |
| postgres | Immutable GET | 1 | 256 | 0 | 0.144 | 8279.4 | 0.0 |
| postgres | ref CAS, 1 repo | 8 | 235 | 21 | 32.636 | 2395.7 | 9.5 |
| postgres | ref CAS, 8 repos | 8 | 249 | 7 | 7.993 | 3671.9 | 3.3 |
| tikv | Immutable PUT | 1 | 256 | 0 | 2.284 | 504.9 | 0.0 |
| tikv | Immutable GET | 1 | 256 | 0 | 0.454 | 4776.4 | 0.0 |
| tikv | ref CAS, 1 repo | 8 | 196 | 60 | 3000.337 | 19.3 | 209.8 |
| tikv | ref CAS, 8 repos | 8 | 254 | 2 | 10.485 | 1052.2 | 6.5 |

p95 covers every first-try response including Retries. Ack/s divides the number of first-try successes / confirmed replays by measurement time,
and drain is the separate time to resend first-try Retries sequentially with the same key. After resends, the contents and logs of all 256 are verified.
With 1 repo, TiKV showed notable counter contention and lock-resolution waits, which improved with the 8-repo placement.
This is a single observation of this prototype including key placement, retry strategy, and 2PC settings. It cannot be generalized into a performance comparison.

First responses for 16 concurrent mixed CASes were `{'Applied': 1, 'Mismatch': 15}` for PostgreSQL and
`{'Applied': 1, 'Mismatch': 8, 'Retry': 7}` for TiKV. After resends, a single publication each and agreement of both kinds of logs were confirmed.

Observed replacement took 14.766 seconds for the data leader and 12.066 seconds for the PD leader.
On quorum loss, Retry came after about 2.001 seconds. After recovery,
it landed with the original key and expected after 5 bounded Retries.
Counts and times are stored in [each recovery response](kv-evaluation/tikv-recover.json).

After the full restart, digests / counts of 1,853 stable records for PostgreSQL and 1,857 for TiKV
matched the originals, and every ref / counter / log / receipt from failure handling also matched.
See [TiKV full verification](kv-evaluation/tikv-verify.json) and [PostgreSQL full verification](kv-evaluation/postgres-verify-baseline.json).

At the time of `docker stats`, each TiKV used about 2.2 GiB, the 3 PDs about 0.2 GiB in total,
and the comparison PostgreSQL about 0.08 GiB. These are point-in-time values on a small dataset, not capacity estimates or peaks.
The original output is stored in the [resource sample](kv-evaluation/resource-sample.log).

## Decision

PostgreSQL / S3, verified with real servers, is currently the default shared storage.
Before adding a production TiKV adapter, the following are needed.

- Real-server E2E covering every Rust storage trait, checked-result fences, webhook cursors / pending bodies, and resends of unknown commit results.
- A policy for storing large metadata, and countermeasures for repo key placement and contention on ordered log counters.
- Additional verification of TLS / authorization, multi-host equivalent HA configurations, long-running load, backup / restore, and failure timing.

FoundationDB is not included in this comparison.
FoundationDB's performance or suitability is not inferred from the TiKV observations.
