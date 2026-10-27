# Git Smart HTTP Reads

English | [日本語](git-read_ja.md)

Git is served on the same `kasu-server` HTTP port as the Metadata API.

```sh
cargo run -p kasu-ingest -- /path/to/source.git /path/to/kasu-data my-repo
KASU_TOKEN=dev-token cargo run -p kasu-server -- /path/to/kasu-data my-repo

git -c http.extraHeader='Authorization: Bearer dev-token' \
  ls-remote http://127.0.0.1:50080/my-repo.git
git clone --depth=1 http://git:dev-token@127.0.0.1:50080/my-repo.git
```

The Basic user name is arbitrary and the password is treated as a PAT. Bearer also works.
When authentication is configured, it is checked before ref advertisement and a Basic challenge is returned. Operator PAT authorization is per the repository specified at startup.
A [projection-only PAT](projection-accounts.md) can read only projections of allowed directories.

## Supported Traffic

- Smart HTTP `info/refs?service=git-upload-pack` and `git-upload-pack`.
- Protocol v0 / v1 ref advertisement and ACKs; v2's separate capability advertisement, `ls-refs`, and `fetch`.
- HEAD symref, v2 unborn, peeled OIDs for tags, ref-prefix.
- Normal clone, depth=1, incremental fetch, deepen / unshallow, shallow boundaries by time and ref.
- Multiple wants, history along all parents, trees / blobs referenced only by intermediate commits, include-tag.
- Wants by SHA reachable from the repository's published refs. Explicitly published ref-less commits can also be fetched.
- Partial clone with `filter blob:none`, follow-up promisor blob fetches, sparse checkout, incremental fetch, unshallow.
- [Path projection URLs](git-projection.md) rooted at a directory. Read-only, with per-projection history and OID index.
- Validation of pkt-line lengths, special markers, and request counts. Unknown capabilities and unsupported arguments are rejected.
- gzip requests. A 16 MiB request size limit both before and after decompression.

`filter` offers only `blob:none`; unsupported filters such as `blob:limit`, `tree`, and `combine` are explicitly rejected.
`no-done`, `sideband-all`, SSH, and LFS are not yet advertised or provided.
For receive-pack `atomic`, see [writes](git-push.md).
For receive-pack scope and write policies, see [Git writes](git-push.md).

Packs are generated from the native store. Server processing never runs `git upload-pack` or keeps a separate Git repository.
Non-delta zlib packs are streamed over a bounded channel, and writing stops when the client disconnects.
The LRU of compressed objects has a 32 MiB accounted capacity limit, counting the payload plus 256 bytes of overhead per object.
Cache keys include the Git OID and the raw object's BLAKE3 ID. Hits, misses, and capacity can be obtained with
`Backend::git_cache_stats()`.
There is no cache that stores whole packs yet.

## Authorization and Pack Sets

The OID index is scoped per repository. Unpublished Changesets that merely exist in the index, and OIDs from other repositories, cannot be wanted.
Explicit publication is done server-side with `GitIndex::publish_commit`, which durably publishes the commit and its entire history, trees, and blobs.
This allows fetching omitted blobs after a partial fetch of a ref-less commit.
There is no feature yet to change publication state over HTTP.
Commit reachability is checked with all-parent ancestry using Generation.

When computing the set the client certainly has from common commits, the parent search stops at the client's existing shallow boundary.
Normal full-history fetches use the [durable reachability index](benchmarks/pack-reachability.md), which stores radix trees shared with parent sets.
History and Git tree bodies already covered by the index are not rescanned, and the set for multiple commons is subtracted exactly from the reachable set of multiple wants.
Underived commits are computed iteratively from all parents, without rescanning subtrees already in the parents' history.
Contents present only in intermediate versions or second parents are also stored, so nothing is missing for incremental reverts or merges.
Authorization of want / have publication scope, and checks of shallow boundaries, thin pack bases, and existing dependencies on push, use the same index without a full history walk per request.
Shallow / deepen requests use the existing bounded graph enumeration.
Shallow merge boundaries cut all parents, matching Git semantics.
Large-scale index backfill / capacity / multi-node load, improving shallow enumeration and negotiation, and delta evaluation on large repositories remain.

## Partial clone

```sh
git clone --filter=blob:none --no-checkout http://git:dev-token@127.0.0.1:50080/my-repo.git
cd my-repo
git sparse-checkout set deploy
git checkout main
```

The initial fetch delivers commits / trees / tags and omits blobs. Blobs needed for checkout or `git show`
are fetched by Git from the promisor remote by OID. Combining with `--depth=1` and `fetch --unshallow` is supported.
v0 / v1 advertise `filter` and `allow-reachable-sha1-in-want`; v2 advertises `fetch=shallow filter`.
Explicit wants take precedence over subtraction by filters or common commits.
Follow-up fetches are subject to the same repository authorization and publication scope, and unpublished blobs that merely exist in the index cannot be fetched.
There is not yet any guarantee of retaining or reading objects that become unreachable after ref deletion or force push.

Full-history and shallow over v0 / v1 / v2 were verified with Git 2.54.0 / macOS arm64 and Git 2.39.5 / Linux arm64 with pinned Rust 1.90.
No blobs in the physical object DB initially, clone with `transfer.fsckObjects=true`,
fetching an old blob on its own, sparse checkout, unshallow, 3 incremental fetches, and matching reachable OIDs and strict fsck after fetching every blob all pass.
Traffic fixtures are stored in the [compatibility matrix](../crates/kasu-git/tests/fixtures/clients/matrix.json),
and verification scope and source / traffic hashes in the [results](../crates/kasu-git/tests/fixtures/clients/partial-clone-summary.json).
Bandwidth and enumeration performance on large repositories, and other filters, are future work.

```sh
KASU_CAPTURE_DIR=/tmp/kasu-partial-traces \
  cargo test -p kasu-server --test git_http partial_clone --locked
docker build -f tools/git-compat/Dockerfile -t kasu:partial-clone-test .
```

The Docker verification runs Git set / parser, HTTP, push, and multi-ref storage contract tests on Linux with pinned Rust 1.90.
Ignored tests that need an external go-git binary are not run by this Dockerfile.

## Basis for Compatibility

Traffic fixtures are stored in the same directory as the [versioned matrix](../crates/kasu-git/tests/fixtures/clients/matrix.json).

| Client | Verified |
|---|---|
| Git 2.54.0 / darwin arm64 | v0 / v1 / v2, ls-remote, full-history / depth=1 bare clone, incremental fetch, deepen / unshallow, time / ref boundaries, strict fsck |
| go-git v5.19.3 / Go 1.27.3 / linux arm64 | HTTP Basic, normal / depth=1 clone and checkout, symlinks / executable bit / non-UTF-8 paths, strict fsck of the fetched Git directory |
| ArgoCD v3.5.6 / Git 2.53.0 / kind linux arm64 | HTTP Basic, v2 fetch, initial sync and resync after ref updates, Application Synced / Healthy, matching SHA and ConfigMap |
| Flux v2.9.6 / source-controller v1.9.6 / kind linux arm64 | HTTP Basic, v0 depth=1, initial sync and resync after ref updates, GitRepository / Kustomization Ready, matching SHA and ConfigMap |

Generated pack tests use `git index-pack --strict` and `git fsck --strict`.
HTTP E2E also checks the full object set and OIDs after clone/fetch, and shallow history length.
There are also tests for intermediate blobs, second parents, shallow haves, unpublished wants, explicit publication, invalid pkt-lines, and the LRU limit.
Two branches with divergence and merges plus an annotated tag are updated 8 times, and full-history / shallow clients over v0 / v1 / v2
verify a multi-ref fetch, strict fsck, and tip OIDs each time. Full-history clients also compare every reachable OID.
The [pack size comparison on the pinned Flux sample](benchmarks/gitops-pack.md) measures non-delta vs. delta on the same OID set.

macOS refuses to check out a raw `0xff` file name, so the same fixture was checked out in a Linux container.
The Go client checks each file's content, symlinks, and executable bits after checkout against the original tree.
Only `.git` and verification artifacts with ASCII names are copied back to the host.

Normal Rust tests:

```sh
cargo test --workspace --all-targets
```

Enable go-git E2E on Linux:

```sh
go -C tools/git-compat build -o /tmp/kasu-go-git-compat .
KASU_GO_GIT_CLIENT=/tmp/kasu-go-git-compat \
  cargo test -p kasu-server --test git_http -- --include-ignored
```

Running on Linux from macOS arm64 (requires Docker and Python 3.12 or later):

```sh
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go -C tools/git-compat build -o /tmp/kasu-go-git-compat-linux .
docker pull rust:1.90-bookworm
KASU_GO_GIT_CONTAINER=1 KASU_LINUX_GO_GIT_BINARY=/tmp/kasu-go-git-compat-linux \
  KASU_GO_GIT_CLIENT="$PWD/tools/git-compat/linux-client.py" \
  cargo test -p kasu-server --test git_http -- --include-ignored
```

Setting `KASU_CAPTURE_DIR` saves the traffic of the clients run. Go sideband contents are omitted, and Git timestamps / PIDs are removed.
Direct execution on Linux was added to the nightly CI `git-read.yml`. Results of runs on GitHub are unconfirmed.

On 2026-10-23, syncing with both controllers was verified on kind v0.33.0 / Kubernetes v1.37.0.
The [procedure and nightly CI](gitops-e2e.md) and [results and real image IDs](../crates/kasu-git/tests/fixtures/clients/gitops-kind-summary.json) are stored.
See M7 for Image Updater / Image Automation write-back and webhooks.
[Path projections](git-projection.md) that serve only a given directory are also provided.

Primary protocol references: [Smart HTTP](https://git-scm.com/docs/http-protocol),
[pack protocol](https://git-scm.com/docs/pack-protocol), [protocol v2](https://git-scm.com/docs/protocol-v2).
The partial clone design follows [promisors and follow-up fetches](https://git-scm.com/docs/partial-clone).
