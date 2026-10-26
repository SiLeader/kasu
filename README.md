# kasu

<img src="kasu-icon.svg" alt="kasu icon" width="160" height="160">

English | [日本語](README_ja.md)

kasu is a server-centric version control system written in Rust that implements the mechanisms described for Anthropic's **Scion** VCS. It provides read-only Git access so existing GitOps tools, including Argo CD and Flux, can consume repositories through familiar Git protocols.

The design follows Scion's server-side operations, incremental changesets, derived data, sparse working copies, and tiered object caches. See Anthropic's [Redesigning source control at Anthropic](https://www.anthropic.com/research/redesigning-source-control) for the original design.

## Why “kasu”?

Git's name can mean a fool or a useless person. In the same spirit, **kasu** takes its name from the Japanese word **カス**: literally “dregs” or “scraps,” and, when applied to a person, something like “a useless person.” It is a common Japanese word with a slangy, derogatory feel in that use. The name is pronounced **カス** (*ka-su*), just as in Japanese.

## How it works

- **Server-side version control:** the server creates changesets and performs merge, rebase, diff, blame, and history queries. Native clients work online and fetch the objects they need.
- **Incremental storage:** immutable changesets record file changes relative to their first parent. Content-addressed objects use BLAKE3 IDs; ref updates use compare-and-swap separately from changeset creation.
- **Derived data:** trees, history indexes, blame, and Git projections are computed incrementally, either eagerly or on demand.
- **Sparse working copies:** the native CLI retrieves selected trees and blobs. Linux also supports virtual checkouts with FUSE and OverlayFS.
- **Tiered object access:** the CLI can use a node-local disk daemon and a shared memory cache, all speaking the same Objects RPC.
- **Storage backends:** SQLite and local object storage for development; PostgreSQL and S3-compatible storage for deployments with multiple servers.

Metadata and write operations are exposed through gRPC and HTTP/JSON, so automation can query and modify repositories without materializing a working copy.

## GitOps interoperability

Git access is a projection of kasu's native model. The gateway generates Git objects and streams packs without maintaining a separate Git repository.

- `git clone`, `git fetch`, and `git ls-remote` over Smart HTTP, with protocol v0/v1/v2 support.
- Shallow clones, incremental fetches, deepen/unshallow, and `blob:none` partial clones.
- Read-only directory projections that expose a selected directory as the repository root.
- Git import with preservation of supported source objects and their SHA-1 OIDs.
- Integration tests with Git, go-git, Argo CD, and Flux; see the [Git read documentation](docs/git-read.md) and [GitOps E2E records](docs/gitops-e2e.md) for the tested scope.

The implementation also supports [policy-controlled Git push](docs/git-push.md) for GitOps write-back workflows such as image automation. Read-only directory projections remain read-only. Account permissions and write policies are documented in [repository accounts](docs/repository-accounts.md).

## Try it locally

You need Rust stable, a C compiler (for SQLite and zstd), `protoc`, and Git. Install `protoc` with `protobuf-compiler` on Ubuntu or Homebrew's `protobuf` on macOS. The native CLI targets macOS and Linux.

From this repository's root, import an existing Git repository with complete history and start the server:

```sh
cargo run -p kasu-ingest -- /path/to/source.git /path/to/kasu-data my-repo
KASU_TOKEN=dev-token cargo run -p kasu-server -- /path/to/kasu-data my-repo
```

The default listeners are `127.0.0.1:50051` for gRPC and `127.0.0.1:50080` for HTTP. In another terminal, read the repository using Git:

```sh
git -c http.extraHeader='Authorization: Bearer dev-token' ls-remote http://127.0.0.1:50080/my-repo.git
git -c http.extraHeader='Authorization: Bearer dev-token' clone --depth=1 http://127.0.0.1:50080/my-repo.git git-work
```

To use a native working copy, build the CLI and connect to the same server. Replace `main` with a branch present in the imported repository:

```sh
cargo build -p kasu-cli --locked
export KASU_SERVER=http://127.0.0.1:50051
export KASU_REPO=my-repo
export KASU_TOKEN=dev-token
./target/debug/kasu checkout main native-work
./target/debug/kasu -C native-work status
```

See the [native CLI guide](docs/native-cli.md) for editing, committing, rebasing, and landing changes, and the [Git import guide](docs/git-import.md) for supported inputs and limits. `dev-token` is an example credential for local use.

## Documentation and status

The implementation includes the native CLI, server APIs, Git gateway, cache hierarchy, and PostgreSQL/S3 backends. See [implementation status](docs/implementation-status.md) for completed work, verification scope, and remaining work, and the [plan audit](docs/plan-audit.json) for implementation and verification references. Most detailed documentation is currently in Japanese.

| Topic | Documentation |
|---|---|
| Architecture and data model | [Design plan](docs/plan/README.md) |
| Working copies | [Native CLI](docs/native-cli.md), [virtual checkout](docs/virtual-checkout.md) |
| Native APIs | [Metadata](docs/metadata-api.md), [writes](docs/write-api.md), [objects](docs/object-api.md), [Land](docs/land.md) |
| Git compatibility | [Import](docs/git-import.md), [reads](docs/git-read.md), [push](docs/git-push.md), [SSH](docs/git-ssh.md), [external LFS](docs/git-lfs.md) |
| Access control and projections | [Accounts](docs/repository-accounts.md), [directory reads](docs/directory-read-acl.md), [Git projections](docs/git-projection.md), [projection accounts](docs/projection-accounts.md) |
| Storage and caches | [PostgreSQL/S3](docs/production-storage.md), [cache hierarchy](docs/cache-hierarchy.md), [disk cache](docs/disk-cache.md) |
| Operations and automation | [Server limits and metrics](docs/server-runtime.md), [tracing](docs/telemetry.md), [webhooks](docs/webhooks.md), [CI bots](docs/automation.md) |
| Verification | [GitOps E2E](docs/gitops-e2e.md), [simulations](docs/simulations/README.md), [benchmarks](docs/benchmarks/README.md), [KV evaluation](docs/benchmarks/kv-evaluation.md) |

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo deny check
```

`cargo deny` requires `cargo-deny`. Tests requiring external services or fixtures have separate instructions in the relevant guides.

The opt-in 100,000-changeset history benchmark emits CSV timings:

```sh
KASU_BENCH_HISTORY=1 cargo bench -p kasu-ops --bench history
```

## License

[Apache License 2.0](LICENSE).
