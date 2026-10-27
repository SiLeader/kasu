# Measuring 100,000 Changesets with Divergence and Merges

English | [日本語](divergent-history_ja.md)

This page records measurements from the FirstParentSkip v1 era. It has since been changed to [v2's fixed-lookup structure](first-parent-skip-v2.md),
with remeasurements on the same history and content IDs added. The CSVs below are kept for comparison.

The M1 synthetic history was extended with second parents that actually diverge.
On macOS arm64 (Rust 1.98.1) and Linux arm64 with pinned Rust 1.90.0,
100,000 Changesets including 10,000 divergent merge Changesets were created.
The final Changeset ID and Tree ID match on both OSes.

In the [existing measurement](README.md), the second parent was an ancestor on the first-parent chain.
This time, tips with different tree changes on each side are merged.
An unnecessary ancestor walk in MergeBase found during this work was also fixed.

## Workload and Measurement Scope

Each cycle of 10 Changesets creates 8 main Changesets (the first cycle includes the root),
1 side, and 1 two-parent merge.
side branches from main 3 generations back and changes `bench/side`.
main updates `bench/main`. The merge has main as first parent and side as second parent,
recording side's change as the diff against the first parent.
For each merge it checks that the tips are not ancestors of each other, that the best merge-base is the branch point,
that the second parent is an ancestor of the merge, and that both files' contents are kept.

What is measured is calls to `Operations::create_changeset`.
It includes parent / blob validation, Merkle tree updates, Changeset storage, and eager computation of RootTree / Generation / FirstParentSkip.
Input construction, result checks, and CSV output are excluded from time and store call counts.
The 3-way merge itself, ref CAS, network, and Git conversion are not measured.

The store is file SQLite (WAL / synchronous=FULL) and local FS / zstd.
Trees are fixed-width, and blob bodies reuse 2 kinds.
A counter wrapping the store counts MetaStore get / put-if-absent and BlobStore
get-compressed / put-compressed calls.
These differ from internal SQL statements, OS physical I/O, and the number of objects actually newly stored.
The counters' atomic increments are included in the measured time.

## Results on 2026-10-24

Each CSV row is the average of the preceding 1,000 Changesets. The table below averages 10 rows (10,000 Changesets) each.

| Range | macOS creation time / item | Linux creation time / item | MetaStore get / item (both OSes) | Max get / item in range |
|---|---:|---:|---:|---:|
| 1–10,000 | 416.73 µs | 863.42 µs | 35.479 | 40 |
| 45,001–55,000 | 531.22 µs | 790.30 µs | 39.300 | 42 |
| 90,001–100,000 | 575.45 µs | 836.84 µs | 40.300 | 43 |

MetaStore put averaged 4.00 per row, and BlobStore get / put averaged 3.00 / 2.00.
The overall maximum MetaStore get was 43.
No growth in call counts corresponding to a linear walk of the whole history was observed.
On the other hand, reads grow with history length.
[Building FirstParentSkip](../../crates/kasu-derived/src/types.rs:83) reads the skip table of the ancestor at each distance,
so at the time there was processing and storage proportional to the logarithm of history length. SQLite indexes are also size-dependent.
This result does not prove or achieve M1's condition of being "strictly independent of history length".

Creation times were measured once in each environment, with builds and other tests running at times.
Storage conditions differ between Docker and macOS, so they cannot be used to compare OSes or production throughput.
This case reuses the same 2 kinds of blobs and a small tree.
Wide trees, large numbers of changes, many distinct large blobs, and many concurrent writers are out of scope.

The [macOS CSV](divergent-history/macos.csv), [Linux CSV](divergent-history/linux.csv), and
[final IDs, aggregates, and source hashes](divergent-history/summary.json) are stored.

## MergeBase Search Termination

Previously, even after finding the best common ancestors, the walk continued from those ancestors to the root.
The new implementation tracks the set of frontiers that could still become best bases,
and terminates when every remaining candidate has become an ancestor of an already found base.

While other live frontiers remain, stale marks are propagated to ancestors even for old candidates.
It does not stop at the first common ancestor, nor unconditionally skip stale nodes.
This preserves multiple best bases in criss-cross merges.

Verification:

- All 1,024 5-node DAGs and all 25,600 tip pairs are checked against every best base enumerated from the transitive closure.
- Short branches are created on main histories of length 10 / 100 / 1,000 / 5,000, using a fresh engine and stored Generation.
  Each merge-base query is checked to make at most 32 MetaStore gets.
- 13 kasu-ops tests pass on both OSes, including the existing random DAGs, 2 and 3 best bases, virtual combination of multiple bases, and line-merge diff tests against Git.
- 16 regression tests for the real Metadata / Writer / account APIs pass on macOS.

These are recorded in the [macOS operation tests](divergent-history/macos-tests.log),
[Linux build and tests](divergent-history/linux-build.log), and
[real API regression tests](divergent-history/server-tests.log).

## Rerunning

```sh
KASU_BENCH_DIVERGENT_HISTORY=1 \
  cargo bench -p kasu-ops --bench divergent_history --locked \
  > /tmp/history-macos.csv 2> /tmp/history-macos.log

docker build -f tools/history-benchmark/Dockerfile -t kasu:history-benchmark .
docker run --rm --network none kasu:history-benchmark \
  > /tmp/history-linux.csv 2> /tmp/history-linux.log
```

`KASU_BENCH_HISTORY_COUNT` accepts multiples of 10 from 10 to 1,000,000, default 100,000.
When included in a normal cargo test / bench without the variable, the benchmark body does not run.
The Docker image already sets the variable that enables measurement.
There are no pass / fail timing thresholds; correctness of results and structural read counts are verified.
The [CI workflow](../../.github/workflows/history-benchmark.yml) runs the same 100,000 and stores the records.
Results of runs on GitHub are unconfirmed.
