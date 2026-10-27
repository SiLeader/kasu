# Changeset Creation vs. History Length Benchmark

English | [日本語](README_ja.md)

The current eager index is [FirstParentSkip v2](first-parent-skip-v2.md).
Index construction at creation is fixed to at most 7 lookups and 82 bytes, verified over 100,000 Changesets on both OSes.
The CSV and the explanation of logarithmic cost below are the measurement record for v1 before the migration.

Additional measurements on [100,000 real branch merges and store call counts](divergent-history.md)
verify matching content IDs on macOS / Linux and the reduction of old-ancestor walks in MergeBase.

For storage candidate comparisons and failure tests, see the [initial TiKV / PostgreSQL evaluation](kv-evaluation.md).

`history-100k.csv` contains results measured on 2026-10-23 on local macOS (aarch64, Rust 1.98.1).
SQLite is a file DB with WAL / synchronous=FULL, and blobs and trees use local FS / zstd.

```sh
KASU_BENCH_HISTORY=1 cargo bench -p kasu-ops --bench history
```

- 100,000 Changesets are created in sequence, each updating `bench/file` between 2 kinds of content.
- Every 10th (except the root) adds a second parent, producing 9,999 two-parent Changesets in the end.
  The second parent is an ancestor on the existing first-parent chain. Correctness of divergence and criss-cross is verified separately by property / unit tests.
- Includes parent and blob existence checks, Merkle tree updates, Changeset storage, and eager computation of RootTree / Generation / FirstParentSkip.
  Ref updates, network, and Git object conversion are not included.
- `mean_create_microseconds` is the creation time of the preceding 1,000 Changesets converted to µs per Changeset.

| Range in history | Mean creation time / Changeset |
|---|---:|
| First 10,000 | 411.42 µs |
| 45,001–55,000 | 669.51 µs |
| Last 10,000 | 580.07 µs |

Even across a range where history grows 100×, no linear increase corresponding to rescanning the whole history was observed.
In the implementation, tree updates are limited to nodes on changed paths, and Generation reads stored parents.
Building FirstParentSkip involves O(log history length) index reads, and SQLite indexes also have size-dependent costs.
So this result does not prove strictly constant time.

This was a single measurement on one machine with other tests running at the same time, and it does not
guarantee production throughput or distributed storage performance. Evaluation of wide trees, many changed paths, real divergence, and multiple servers is future work.
