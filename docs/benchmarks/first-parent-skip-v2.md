# FirstParentSkip v2 and Fixed Lookup Cost at Creation

English | [日本語](first-parent-skip-v2_ja.md)

The FirstParentSkip eager index has been changed to v2.
v1 was a table storing the ancestor at each distance `2^k`, requiring lookups and storage proportional to history length at creation.
v2 stores the first-parent depth, the immediate parent, and one jump.
Index lookups at new creation are at most 7, and records are 10 bytes for the root and 82 bytes otherwise.
The original Changeset / Tree / Git OIDs are unchanged.

## Structure and Queries

The root has depth 0, and a child's depth is its first parent's depth + 1.
The default jump is the immediate parent. When the parent's jump and the jump of its destination have the same distance,
it jumps to the destination of the two combined. The distance from the child is `1 + 2d`, giving lengths 1, 3, 7, 15, ….
The parent's value is received from the scheduler, and at most one additional existing index is read.
For normal eager creation with stored parents, index MetaStore gets including the scheduler are at most 7.

First-parent ancestor queries use jumps that do not overshoot the target depth, and otherwise step to the immediate parent.
On reaching the same depth they compare IDs, so other branches and second parents are never treated as first-parent ancestors.
First-parent depth and Generation, which uses the maximum over all parents, are different values.
All-parent IsAncestor / MergeBase continue to use Generation.

## Coexistence with v1 and Backfill

- The eager Rust type `FirstParentSkip` is stored at `derived/FirstParentSkip@2/...`.
- `FirstParentSkipV1` remains for reading the old `@1` format and for explicit lazy computation.
- `Engine::standard()` registers both versions, but new creation does not build the v1 table.
- v1 values are neither deleted nor overwritten, and the original Changeset IDs and existing ref / log cursor formats are kept.

When existing history has no v2, the first query or new creation backfills the needed first-parent chain
with the iterative scheduler. This first pass depends on history length.
For normal creation to have fixed lookup cost, the parent's v2 index must already be computed.
During migration, it can be computed in advance for each ref tip with the normal Derived Data API.

```rust
engine.derive::<kasu_derived::FirstParentSkip>(&ctx, branch_head, &())?;
```

This only adds immutable Derived Data and does not move refs.
No new dedicated admin RPC / command has been added. Backfill reuses common ancestors computed once.
When the old v1 is explicitly recomputed, its computation and storage remain logarithmic as before.

## 100,000-Changeset Measurement on 2026-10-24

The same 100,000 Changesets with 10,000 divergent merges as the [divergence and merge benchmark](divergent-history.md) were used.
Measured on macOS arm64 / Rust 1.98.1 and Linux arm64 / pinned Rust 1.90.0.
The final Changeset ID and Tree ID on both OSes also matched the v1 measurement results.
The history and file contents produced by the measurement code are unchanged.

| Range | v1 MetaStore get / item | v2 MetaStore get / item (both OSes) | v2 max get / item in range |
|---|---:|---:|---:|
| 1–10,000 | 35.479 | 24.299 | 27 |
| 45,001–55,000 | 39.300 | 24.300 | 27 |
| 90,001–100,000 | 40.300 | 24.300 | 27 |

The maximum gets for the whole creation process dropped from 43 to 27.
MetaStore put averaged 4.00 in every range, and BlobStore get / put averaged 3.00 / 2.00.
These are store API call counts, not internal SQL statements or OS physical I/O.

| Range | macOS creation time / item | Linux creation time / item |
|---|---:|---:|
| 1–10,000 | 379.80 µs | 531.34 µs |
| 45,001–55,000 | 466.66 µs | 910.88 µs |
| 90,001–100,000 | 451.03 µs | 639.17 µs |

A small fixed tree and 2 kinds of blobs were reused, with SQLite WAL / synchronous=FULL and local FS / zstd.
Input construction, ancestry / merge-base / content checks of the results, and CSV output are excluded from creation time.
The counters' atomic increments are included in the time.
Each OS was measured once, with some other builds and tests running concurrently.

Index construction at creation no longer requires walking or storing a table proportional to existing history length.
However, real time for SQLite B-trees, the filesystem, caches, and disk syncs is not guaranteed to be constant.
Nor does this guarantee performance for wide trees, many changed paths, concurrent writers, or production storage.
M1 evaluation distinguishes application-side lookup counts and index storage at a fixed amount of change from real storage latency.

The [macOS CSV](first-parent-skip-v2/macos.csv), [Linux CSV](first-parent-skip-v2/linux.csv), and
[aggregates, final IDs, and source hashes](first-parent-skip-v2/summary.json) are stored.

## Verification

- Create 10,000 Changesets and check that each creation makes at most 7 v2 index gets, stores 10 / 82 bytes, and makes 0 v1 lookups.
- Check first-parent ancestors at depth boundaries and deterministic samples, with at most 100 index gets per query.
- Distinguish all-parent ancestry from first-parent ancestry for merges with a shallow first parent and a deep second parent.
- Check first-parent decisions against actual parent chains for all 1,024 5-node DAGs and 25,600 tip pairs.
- Check preservation of stored v1 contents after reopening SQLite, v2 backfill, appends, and unchanged Changeset IDs.
- The codec rejects inconsistent depth / parent / jump.

16 kasu-ops tests and 5 selected kasu-derived tests pass on both OSes.
16 real Metadata / Writer / ACL tests and 12 real CLI E2E tests on macOS also pass.
These are recorded in the [operation tests](first-parent-skip-v2/macos-tests.log), [Derived Data](first-parent-skip-v2/derived-tests.log),
[Linux build](first-parent-skip-v2/linux-build.log), [real API](first-parent-skip-v2/server-tests.log), and
[CLI](first-parent-skip-v2/cli-tests.log).

```sh
cargo test -p kasu-ops --test first_parent --test operations --locked
KASU_BENCH_DIVERGENT_HISTORY=1 \
  cargo bench -p kasu-ops --bench divergent_history --locked
docker build -f tools/history-benchmark/Dockerfile -t kasu:skip-v2-test .
docker run --rm --network none kasu:skip-v2-test
```

[CI](../../.github/workflows/history-benchmark.yml) also runs migration and DAG verification and the 100,000-Changeset measurement.
Results of runs on GitHub are unconfirmed. The old measurement CSVs are kept as records of v1 at the time.
