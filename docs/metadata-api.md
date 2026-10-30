# Metadata API

English | [日本語](metadata-api_ja.md)

You can import an existing Git repository and query it without a checkout.
Building requires Rust stable, a C compiler, and `protoc`.

```sh
cargo run -p kasu-ingest -- /path/to/repo.git /path/to/kasu-data my-repo
KASU_TOKEN=dev-token cargo run -p kasu-server -- /path/to/kasu-data my-repo
```

The default listeners are gRPC `127.0.0.1:50051` and HTTP `127.0.0.1:50080`.
Any addresses can be given as the third and fourth arguments. Use `RUST_LOG` to set the log level.
When `KASU_TOKEN` is set, both transports require `Authorization: Bearer <token>`.
When it is unset, the server runs in an unauthenticated development configuration. The current unit of authorization is the one repository specified at startup.

## gRPC

The canonical definition is `kasu.v1.Metadata` in [kasu.proto](../crates/kasu-proto/proto/kasu.proto).
Rust clients can connect with `kasu_proto::v1::metadata_client::MetadataClient`.
IDs are 32-byte BLAKE3 and Git OIDs are 20-byte SHA-1. Paths, signatures, and messages stay as bytes.
For creation, merging, and ref updates through `kasu.v1.Writer` on the same gRPC port, see the [Write API](write-api.md).

| RPC | Behavior |
|---|---|
| `GetChangeset` / `GetRootTree` | Changeset body / root Merkle tree ID |
| `ResolveRef` / `ListRefs` | Native ref target and CAS version. Imported branch names are `refs/heads/main` and so on |
| `Log` | First-parent log. With a path, returns only Changesets where that path / directory changed in the diff against the first parent |
| `IsAncestor` / `MergeBase` | Ancestry check over all parents / returns every best common ancestor |
| `Diff` | Per-path before / after Entry. With content=true, also returns line-level change ranges |
| `Blame` | Originating Changeset, path, and original 1-based line number of each line. Lines unchanged from all parents are inherited |
| `ReadFile` | Entry and content of a File / Symlink. Gitlink contents are not fetched |
| `ListDir` | Immediate entries. An empty path is the root. Child trees are returned by ID |
| `FindChangesetByGitOid` | Reverse lookup of a commit through this repository's Git OID index. No id when not found |
| `FindChangesetsByPrefix` | Changesets of this repository whose hex ID starts with `prefix` (4–64 hex digits), at most 10; more than one means the prefix is ambiguous. Used for short IDs |

`Log.limit` is 1–1000. For the next page, pass the same start / path and the returned cursor.
The cursor is also checked against the first-parent chain and the path's change points. Paging by ID is unaffected by ref movement.
The cursor is empty at the end. Path tracking is name-based.

Diff paths select each path itself and everything under it. The start of a content change range is 0-based; before / after are lists of byte strings
including line endings. A trailing line without a newline is preserved. Content containing NUL is binary=true and no line diff is returned.
When only a file's mode changes, the Entry diff is returned and the content change ranges are empty.

PathHistory and Blame are Lazy Derived Data with the path as SubKey.
Blame also computes the parent's rename source path when needed. `PathRequest.follow_moves=true` tracks line moves within the same file, and
`follow_copies=true` tracks moves plus whole / partial copies from other files. Both default to false.
Generation rules, Git comparisons, and supported scope are recorded in [CopyDetect / PathIndex and line tracking](blame-lineage.md).

## HTTP/JSON

POST the same messages as JSON. bytes are **arrays of integers 0–255**, and oneofs are kind objects.
Field names are snake_case as in protobuf. Omitted fields use their default values.

| Path | Request type |
|---|---|
| `/v1/changeset` / `/v1/root-tree` | ChangesetRequest |
| `/v1/resolve-ref` | RefRequest |
| `/v1/refs` | ListRefsRequest |
| `/v1/log` | LogRequest |
| `/v1/is-ancestor` / `/v1/merge-base` | CompareRequest |
| `/v1/diff` | DiffRequest |
| `/v1/blame` / `/v1/read-file` / `/v1/list-dir` | PathRequest |
| `/v1/find-git-oid` | GitOidRequest |
| `/v1/find-prefix` | PrefixRequest |

Example: list refs of `my-repo`.

```sh
curl -s http://127.0.0.1:50080/v1/refs \
  -H 'Authorization: Bearer dev-token' \
  -H 'Content-Type: application/json' \
  --data '{"repo":[109,121,45,114,101,112,111],"prefix":[]}'
```

Invalid input returns 400, unauthenticated 401, another repository 403, and a missing Changeset / file 404.
Internal storage error details are not returned to clients; they are recorded in tracing.
The limit for HTTP requests and received gRPC messages is 16 MiB, and for gRPC responses 256 MiB.

## Repository Isolation and Verification

Changesets are checked against immutable repository membership records. Even if another repository's ID is stored in the same store,
it cannot be used in queries for this repository. Membership is recorded at import and filled in on reimport.

Tests compare against `git log --first-parent -- <path>`, `git blame --line-porcelain`,
`git merge-base --all`, and `git diff --no-renames --unified=0` on generated Git histories,
including deletion, re-adding, mode changes, repeated lines, two-parent merges, and criss-cross merges.
[Renames / copies and line fragments](blame-lineage.md) are compared in 3 modes × 24 cases covering pure / edited renames, whole-file / line-fragment copies, and line moves with later edits.
In the [Metadata verification against a pinned real OSS repository](metadata-oss.md), all 189 path histories, 77 Blames, and 441 merge-base pairs across all 21 commits matched on macOS / Linux.
Parity with every Git rename / copy heuristic and with arbitrary OSS repositories in general is remaining work.
Real gRPC traffic, HTTP JSON, the server binary with a persistent store, and authentication / membership / cursors are also verified.

[Git Smart HTTP reads](git-read.md) are served on the same HTTP port.
The [Write API](write-api.md), [Git writes](git-push.md), [Object RPC](object-api.md), and [projection read ACLs](projection-accounts.md) are also implemented,
as are [native directory ACLs](directory-read-acl.md), [telemetry](telemetry.md), and [authoritative server load limits](server-runtime.md).

`GetBlobMetadata(ObjectRequest)` and `/v1/blob-metadata` return the size and Git blob OID of up to 128 blob IDs belonging to the same repository.
They do not read bodies. They are used for attribute queries by the [virtual checkout](virtual-checkout.md).

## Per-directory Read Views

For an account with [read_directories](directory-read-acl.md), GetChangeset / Log return only changes under allowed paths,
omit raw Git provenance, and set `read_filtered=true`. The id references the original Changeset.
The original Changeset's canonical bytes cannot be reconstructed from this response.
GetRootTree returns a canonical TreeId built only from the allowed scope, with `read_filtered=true`.
The same restrictions apply to ListDir / ReadFile / Diff / Blame / GetBlobMetadata.
The storage format and IDs of the original Changesets / Trees are unchanged. For unrestricted accounts the flag is false.
