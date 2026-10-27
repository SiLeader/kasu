# Durable Reachability Index for Git Packs

English | [日本語](pack-reachability_ja.md)

An immutable radix tree that stores the reachable set of Git OIDs has been added for normal full-history fetches.
Advertised capabilities, Git object contents / OIDs, and the non-delta pack format are unchanged.
Requests with shallow history, deepen, or time / ref boundaries use the existing bounded enumeration.

## Computation and Storage

`git/reachability/v1/<projection>/<git-oid>` points to the root of a reachable set.
Nodes are stored with put-if-absent at `git/reachability-node/v1/<BLAKE3>`.
The tree has 16-way branching on OID hex digits, sorted leaves of at most 64 OIDs, and at most 40 levels.
The same set yields the same root regardless of insertion order or parent order.

All parents of a commit are derived iteratively, and the parents' reachable sets are merged.
When walking a new root tree, subtrees already included in the parents' history are skipped.
Old blobs deleted from the current snapshot also remain in the set, so intermediate versions, reverts, and second-parent contents are handled.
Blob contents are not read for enumeration. Gitlink targets are not included.
The first derivation reads unprocessed history and changed trees.

On fetch, the stored roots of multiple wants and commons are compared, skipping identical nodes.
Temporary unions for each combination of client-specified haves are not persisted.
The in-request node LRU is limited to 1,024 nodes, and the commit DAG is not walked recursively.
Index roots are published after their dependent nodes are stored, so they can be recomputed after an interruption.
Roots are separated per repo / path projection and used after the existing want / have authorization.

## Measurements

On 2026-10-24 (JST), on macOS arm64 / Rust 1.98.1 / Git 2.54.0 with a release build,
a SQLite file DB / FS blob store, and the same pinned Flux sample SHA `4f1a5b1051b469108d1bdf0ebec6c814427e3834`
as the [first measurement](gitops-pack.md).
124 commits / 588 objects.

| Request | Objects | First enumeration | Re-enumerating the same request | kasu pack |
|---|---:|---:|---:|---:|
| Full history | 588 | 167.665 ms | 0.097 ms | 735,303 B |
| depth=1 | 46 | 3.343 ms | 0.757 ms | 26,158 B |
| 1-commit increment | 11 | 0.266 ms | 0.157 ms | 15,140 B |
| 10-commit increment | 123 | 0.507 ms | 0.439 ms | 77,308 B |

Cases run in order from the top. The first full-history enumeration includes building the index,
and the incremental cases reuse it. depth=1 does not use the reachability index.
Re-enumeration also creates a new in-request LRU, but SQLite / OS caches are warm.
The 1-commit increment in the first measurement was about 76 ms. The run time and cache conditions also differ,
so production performance or an exact speed ratio is not judged from this difference alone.

The [CSV](gitops-pack-reachability.csv) stores all SHAs and compression / enumeration times.
Full-history / incremental OID sets matched real Git's `rev-list --objects`, and every pack passed
`git index-pack --strict --stdin`. kasu pack object counts and sizes are the same as the first measurement.
This is a small measurement of one source with one run per condition. Large-scale index backfill, storage capacity,
load with many caches / servers, and shallow enumeration are future evaluation and improvement targets.

## Verifying Correctness and Read Volume

- Proptests comparing radix tree union / difference / membership against ordered sets, including long common OID prefixes and multiple commons.
- On DAGs with random divergence, merges, and reuse of identical blobs, multiple wants / haves and `blob:none` are compared against a full walk.
- After deriving a history of 64 directories, 512 files, and 129 commits, checks that a single change needs only 3 body reads:
  the new commit and the changed root / directory trees.
  With a separate SQLite connection and a new index instance, the same enumeration needs 0 body reads.
- Existing E2E rerun for v0 / v1 / v2 normal / shallow / partial / path projection, 8 consecutive fetches with divergence, merges, and multiple refs,
  push, and strict verification of each pack / repository.

The macOS [Git tests](pack-reachability-git.log) and [real HTTP tests](pack-reachability-http.log),
and the [Docker tests and source hashes](pack-reachability-linux.log) on Linux arm64 with pinned Rust 1.90 / Git 2.39.5 are stored.
Ignored tests that externally specify the pinned Go client are not included in this run.

```sh
cargo test -p kasu-git --locked
cargo test -p kasu-server --test git_http --test push --locked
cargo clippy -p kasu-git -p kasu-ingest --all-targets --locked -- -D warnings
docker build -f tools/git-compat/Dockerfile -t kasu:reachability-test .
```

To reproduce the size measurements, use the [pinned source procedure](gitops-pack.md).
