# Native Write API

English | [日本語](write-api_ja.md)

`kasu-server` provides `kasu.v1.Writer` on the same gRPC port as Metadata.
HTTP JSON is also available on the same HTTP port.
When `KASU_TOKEN` is set, a Bearer or Basic token is required.
Authorization targets the one repository specified at startup.

| RPC | HTTP POST | Behavior |
|---|---|---|
| `UploadBlobs(stream BlobUpload)` | `/v1/upload-blobs` | Chunked upload of raw blobs; existing blobs can be skipped |
| `CreateChangeset` | `/v1/create-changeset` | Creates after validating parent and blob membership, changed paths, and contents |
| `Merge` | `/v1/merge` | 3-way merge using every best merge-base |
| `Rebase` | `/v1/rebase` | Reapplies one Changeset's diff against its first parent onto `onto` |
| `UpdateRef` | `/v1/update-ref` | CAS after validating ref policy and fast-forward |
| `Land` | `/v1/land` | rebase + CAS with a durable idempotency key, limits, backoff, and cancellation |

The schema is [`kasu.proto`](../crates/kasu-proto/proto/kasu.proto).
As with the existing Metadata API, byte strings are protobuf `bytes` and numeric arrays in HTTP JSON.
author / committer / message, paths, and conflict contents are not converted to UTF-8.
Changes in `CreateChangeset` must be sorted by path and unique.
There is no field for clients to specify Git provenance.

## Uploading Blobs

Send `BlobUpload` with `repo`, the expected raw BLAKE3 `id`, `offset`, `data`, and `last`.
Send the chunks of one blob consecutively; offset starts at 0 and advances by the length of data sent.
The hash is verified and the blob stored on the final chunk. An empty blob is empty data, offset=0, last=true.
Early termination, offset mismatches, switching IDs, and hash mismatches are errors.
Blobs already completed remain as immutable objects even if a later blob fails.

A blob already stored in the same repository can be existence-checked with empty data, offset=0, last=true to skip sending it.
The resulting `BlobReceipt` returns the ID, actual size, and `existing`.
A blob that exists only in another repository cannot be skipped; its content must be uploaded to prove the hash.
Parent Changesets and the blob of each Set are checked for repository membership.
Git import also records blob membership. For blobs without membership records in older databases, reimport or upload them.

gRPC limits are 16 MiB per message, 256 MiB per blob, 512 MiB of data per stream,
and at most 4,096 blobs / 65,536 chunks. The server assembles one blob at a time.
Over HTTP, send `{ "chunks": [...] }`; the whole JSON request is limited to 16 MiB.

## Merge / Rebase and Conflicts

A `RewriteResponse.id` means success. A fast-forward Merge returns the existing ID.
On conflict there is no ID, and `stage`, `bases`, and `conflicts` return the conflicts of the base or the final tree.
Conflict entries have the path, kind, base / ours / theirs, and line ranges and contents.
Successful Merge / Rebase does not move refs either.
See [merge semantics and tests](native-merge.md).

## Updating Refs

`UpdateRefRequest` specifies the name, the required expected Ref (name, target, version), and the new target.
A new ref starts with no target and version=0.
A CAS mismatch is not an error; it returns `applied=false` and the actual state.
The version keeps increasing monotonically after deletion, rejecting ABA through delete and recreate.
A successful CAS and the ref_log append are in the same transaction.

By default only creating and fast-forwarding `refs/heads/` is allowed.
Force and deletion are disabled. When embedding the server, `Backend::with_write_policy` configures
writable prefixes, force-allowed prefixes, and whether creation / deletion is allowed.
A non-fast-forward requires both `force=true` in the request and force permission on the server.
The CLI can load JSON settings from `KASU_WRITE_POLICY_FILE`.
See [Git writes](git-push.md) for `*` / `?` ref patterns, byte-string path boundaries, and the final-tree ACL shared with Git.

The only mutable state of a Git branch is the native ref.
The immutable Git commit projection is prepared before the CAS, rejecting signatures and other values Git cannot represent.
Git advertisements after the CAS reference the updated native ref.

Verification covers gRPC streams and HTTP JSON, authentication and repository isolation, CAS / ABA / force / delete policies,
and Git clone plus `fsck --strict` of history created through the API.

```sh
cargo test -p kasu-server --test write --locked
```

See [Land](land.md) for Land requests, final results, resends, and cancellation.
