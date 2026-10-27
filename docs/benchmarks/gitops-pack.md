# Pack Sizes for a GitOps Repository

English | [日本語](gitops-pack_ja.md)

Measured on 2026-10-23 with [fluxcd/flux2-kustomize-helm-example](https://github.com/fluxcd/flux2-kustomize-helm-example/tree/4f1a5b1051b469108d1bdf0ebec6c814427e3834)
pinned at `4f1a5b1051b469108d1bdf0ebec6c814427e3834`.
It is a small sample of 124 commits / 588 objects. macOS arm64, Rust 1.98.1, Git 2.54.0, a release build,
a SQLite file DB, and an FS blob store were used.

| Request | Objects | kasu non-delta | Git non-delta | Git delta |
|---|---:|---:|---:|---:|
| Full history | 588 | 735,303 B | 702,658 B | 526,220 B |
| depth=1 | 46 | 26,158 B | 24,096 B | 22,128 B |
| 1-commit increment | 11 | 15,140 B | 13,837 B | 13,837 B |
| 10-commit increment | 123 | 77,308 B | 71,948 B | 41,829 B |

The [CSV](gitops-pack-flux.csv) also stores the pinned SHAs, haves, and enumeration / cold / warm compression times.
These are pack body bytes and exclude HTTP / TLS / pkt-line overhead.
Increments use the first-parent commit 1 / 10 back as the have of a client with full history.

The **same OID set** chosen by `plan_fetch` was packed by kasu and Git.
kasu uses its existing non-delta fast zlib; Git uses `pack.compression=1` without reuse,
with `--window=0` for non-delta and `--window=10 --depth=50` for delta.
The compression implementations and ordering also differ, so differences from Git non-delta include those effects.
Thin packs based on external have objects were not measured.
Every kasu pack passed `git index-pack --strict --stdin` against the original complete repository.

In this sample, the 1-commit increment was the same size with or without Git deltas.
For the 10-commit increment, Git deltas were about 46% smaller than kasu, and about 28% smaller for full history.
For now, non-delta continues to be used for delivering depth=1 and frequent single updates.
For use cases bundling many updates or for large repositories, the effect of introducing deltas will be evaluated further.
The results of one small repository with one run per condition are not used to conclude deltas are unnecessary at large scale.

Enumeration took about 121 ms for full history and about 76 ms even for the 1-commit increment.
The implementation in this first measurement re-enumerated the reachable set of haves, so incremental enumeration time depended on history size.
On 2026-10-24 a durable reachability index for normal fetches was added and [remeasured on the same pinned source](pack-reachability.md).
Shallow enumeration and large-scale backfill / capacity evaluation are still needed. Cold / warm times include compression cache differences,
and the cache is built from empty for each case. This is not a network transfer speed benchmark.

Reproduction steps (use a temporary repository dedicated to this measurement as the destination):

```sh
git clone --bare --single-branch --branch main \
  https://github.com/fluxcd/flux2-kustomize-helm-example.git /tmp/kasu-flux-bandwidth.git
cargo run --release --locked -p kasu-ingest --example pack_bandwidth -- \
  /tmp/kasu-flux-bandwidth.git 4f1a5b1051b469108d1bdf0ebec6c814427e3834
```

The native store for measurement is created in a temporary directory and deleted at exit.
Strict verification adds packs to the input Git repository, so do not specify a repository used for normal work.
