# Daemon Disk Capacity Management

English | [日本語](disk-cache_ja.md)

The `kasu-daemon` disk Objects cache has limits on the capacity of stored zstd frames and the number of typed index entries.
New stores that exceed the limits evict LRU index entries and delete frames whose last index entry is gone.
Deliveries in progress keep using the `Arc<Vec<u8>>` they already obtained, so eviction never changes content being returned.

```sh
kasu-daemon serve --repo my-repo --upstream http://127.0.0.1:50091 \
  --token-file /run/kasu/user-token --upstream-token-file /run/kasu/daemon-hop \
  --data /var/cache/kasu/node --socket /run/kasu/node.sock \
  --disk-bytes 8589934592 --disk-entries 100000 --snapshot-keep 2
```

| Option | Default | Meaning |
|---|---:|---|
| `--disk-bytes` | 8 GiB | Total bytes of cached zstd frames. 1–2^50 |
| `--disk-entries` | 100,000 | Total number of typed Tree / Blob index entries. 1–1,000,000 |
| `--snapshot-keep` | 2 | Number of managed periodic snapshots kept. 1–1000 |
| `--snapshot-interval-secs` | 3600 | Periodic save interval. 0 stops it; at most 1 year |
| `--snapshot-max-bytes` | 8 GiB | Limit on total frames imported at startup. Independent of the cache limit |

Bytes are the logical size of stored payloads. Filesystem block-level allocation, SQLite / indexes / locks,
import staging, snapshot archives, and files placed by users are not included.
When a Tree and a Blob use a frame with the same ID, bytes are counted once and entries are counted for each.
SQLite holds the typed indexes and LRU, without replacing the existing representation of compressed frames.
If a different zstd representation of the same content is received, the first frame is used.

When a single frame is larger than the byte limit, the fetched frame is served without being stored.
The existing cache is not evicted. The next request refetches through the normal cache hierarchy.
Misses after eviction also use the existing daemon → cache → authority singleflight, pacing, concurrency, and circuit breaking,
and never bypass directly to the authoritative server.

## Refetch Control on Storage Failures

A cold miss whose Store save fails returns an error. If the fetch from the upstream succeeds but storage fails,
it is counted as a failure by the fill circuit breaker. After 3 consecutive failures by default, it enters cooldown,
and new cold misses end with `Unavailable` without repeatedly fetching from the upstream.
Requests already fetching may remain, up to the fills limit. Hits on existing frames do not go through this circuit.
After cooldown only one request becomes the probe, and the breaker is released only after both fetch and storage succeed.
A probe that keeps failing to store enters cooldown again.
The normal path of intentionally not storing frames larger than the configured limit still succeeds as before.

The [regression verification record](simulations/cache-write-failures/summary.json) includes reproduction before the fix, 6 hierarchy tests on macOS,
and 43 cache / daemon / CLI tests on Linux with pinned Rust 1.90.
It injects Store::put errors and checks that 30 cold misses after 3 failures do not increase authoritative fetches,
plus warm hits, a failing probe, storage after recovery, and the next hit.
It is not a test that produced ENOSPC / power loss on a real filesystem.

## Durable LRU and Multiple Instances

The formats of `scope.json`, the 8-byte size markers in `index/<kind>/<id>`, and `objects/<first 2 hex digits>/<remaining 62>` are kept.
`cache-lru-v1.sqlite3` and `.cache-lru.lock` are newly used.
Successful gets / puts advance a durable sequence number. Snapshot export does not update the LRU.
A Tree / Blob sharing the same frame share access order, and ties are broken by ID / kind for eviction.
At the sequence number limit, numbers are reassigned keeping the current relative order.

The public API `DiskStore::open_with_limits` sets limits and applies them to existing data too.
`DiskStore::open` uses the stored configuration, and uses defaults only for a new cache.
The manual snapshot command never resets a running daemon's configuration to defaults.
The daemon's serve stores its startup options as the configuration, so specify the same limits when restarting.

Each instance uses SQLite WAL / FULL synchronous and participates in a shared file lock.
Publishing and deleting files are exclusive, and reads / exports are protected from eviction with a shared lock.
Double starts of the same daemon are rejected by the existing daemon / socket lease, but manual exports while running are possible.
The inter-process file lock times out at 5 seconds and SQLite busy waits at 2 seconds.
Disk operations within one instance are serialized, so normal cache fills wait during a large snapshot.
Metrics never take the disk lock, even while waiting.

Specify as the data directory a dedicated empty directory, or a directory with an existing valid cache scope.
A new cache does not adopt a directory containing unrelated existing files, and rejects an authoritative store containing `meta.db`.
On the first start of an existing cache, markers are migrated, and when shrinking to the current limits, LRU eviction runs.
Only trusted owners should manage this directory, and older versions or other programs must not write to it concurrently.
Coordination while metadata is being deleted or modified by hand is out of scope.

## Multi-repository Membership

Separately from frames, the catalog stores `(repo, kind, id)` membership and a repository identifier per name.
A frame without membership is returned for a repository only after confirming with the upstream. Membership for names whose identifier changed
(such as recreating the same name after deletion) is discarded. When a catalog from the single-repository era is opened,
every existing frame is registered as membership of that repository. Snapshots (manifest version 2) include membership,
and `kasu-daemon snapshot --repo` selects target repositories. Version 1 snapshots can also be loaded.

## Interruption and Recovery

Before changing files, a dirty intent is committed to SQLite; markers, frames, and directories are synced, and then
indexes and counters are committed. If the process exits midway, the next operation or open recovers exclusively.

- Rebuild typed indexes from published size markers whose frames exist.
- Reclaim markers without frames and canonical frames not referenced by any marker.
- Reclaim interrupted owned temp files and `.snapshot-import-` staging.
- Recompute capacity and index counts, shrink to the limits, and clear the dirty intent.

Normal reads verify the typed index, size, zstd, and content hash.
Corrupt compressed frames and invalid markers are treated as errors and are not hidden by serving or automatic refetching.
Unpublished frames / missing markers reclaimed by recovery can be refetched as subsequent legitimate cache misses.
Corruption of SQLite itself is never automatically deleted and recreated.

## Snapshots and Generation Management

Periodic saves publish `data/snapshots/kasu-snapshot-v1-<timestamp>.tar` through a temporary file.
After success and at startup, the `--snapshot-keep` files with the newest numeric timestamps are kept.
While writing a new one, capacity for one extra generation is temporarily needed.
Pruning takes the same cache's exclusive lock, so it never deletes a file another export is still writing.
Owned incomplete snapshot temp files are reclaimed too.

`user.tar`, older digit-only `<timestamp>.tar` files, and archives manually saved elsewhere are never deleted.
Old-format periodic archives and users' files are managed by operators. Management file names and directories are reserved.

The snapshot format remains version 1. Import verifies every entry and pre-checks capacity before publishing,
and pins every manifest key so earlier entries are not evicted during the import.
Archives that do not fit within the cache limits are rejected before publication or eviction.
If an existing frame has a different compressed representation, the pre-check uses the actual size of the existing frame that will be kept.
On IO failures after verification, only part of the verified data may remain.

Archives carry a payload per typed entry, so a shared frame belonging to two types is included twice.
Import's `--snapshot-max-bytes` checks this total. Export / import keep the limits of 100,000 entries and a 16 MiB manifest.
If the cache is configured larger than 100,000 entries, a full export that exceeds this limit is an error.

## Metrics and Verification

The existing `kasu_object_entries` is the number of typed index entries, and the daemon's `kasu_object_bytes` is deduplicated frame bytes.
The following metrics are added with `layer="daemon"`.

| Metric | Meaning |
|---|---|
| `kasu_object_disk_limit_bytes`, `kasu_object_disk_limit_entries` | Configured limits |
| `kasu_object_disk_evictions_total` | Typed index entries deleted for capacity |
| `kasu_object_disk_evicted_bytes_total` | Frame bytes freed by deleting the last index entry |
| `kasu_object_disk_admission_skips_total` | Times storing a single frame was skipped |
| `kasu_object_disk_repairs_total` | Missing markers, orphaned frames, and owned temp files reclaimed by recovery |

Cumulative values persist values of committed operations in the catalog; counts from interrupted deletions are not fully restored.
Metrics are approximations from atomic counters that do not take the disk lock.
Updates by other instances are reflected after this instance performs its next operation.
The exact shared state can be queried with `DiskStore::stats()`.

The [verification record](simulations/disk-lru/summary.json) stores logs and source hashes for macOS and Linux with pinned Rust 1.90.
Verification includes reopening the LRU, byte / entry limits, frames shared by types, different compressed representations, oversized frames, legacy migration,
invalid directories, snapshot pins and pre-rejection, concurrent instances / exports, integer boundaries, and metrics not waiting.
Writes are SIGSTOPped to actually observe a mismatch between files and SQLite, then SIGKILLed, and
reopening, verification of every remaining frame, `integrity_check`, and compliance with limits are confirmed.
The real daemon verifies eviction and refetch with a 1-entry cache, warm hits after a forced kill, not storing a single oversized frame,
and the number of periodic snapshot generations and retention of unrelated archives.

This is not a guarantee of capacity or throughput in large-scale production.
Power loss on filesystems, network filesystems, broad IO failures including disk-full, and performance evaluation of huge catalogs / snapshots remain.
