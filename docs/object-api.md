# Object RPC

English | [日本語](object-api_ja.md)

`kasu.v1.Objects` is registered on the same gRPC port as Metadata / Writer.
It provides `GetTrees(ObjectRequest) -> stream TreeObject` and
`GetBlobs(ObjectRequest) -> stream BlobChunk`.
`ObjectRequest` has `repo` and `ids`, the 32-byte IDs to fetch.
Authentication is the same Basic / Bearer as Metadata.

Each chunk has `id`, `offset`, `data`, and `last`.
`data` is a piece of the zstd frame stored in the store, at most 64 KiB.
`offset` is the position within the compressed bytes. Chunks of the same object are contiguous,
and only the last chunk has `last: true`. Objects are delivered in request order.
Trees are chunked too when large. Empty files are also delivered as zstd frames.

Clients should check chunk IDs / offsets / termination, and after obtaining a complete frame
verify the decompression limit, the content's BLAKE3, and the Tree's canonical codec.
A successfully verified frame can be stored in a disk cache without conversion.
The server does not recompress on this delivery path.

## Membership and Fetch Order

Fetch in the order `ResolveRef` → `GetRootTree` → needed child Trees → needed Blobs.
`GetRootTree` registers repository membership indexes for Trees from an authorized Changeset.
The index registers parents last and does not rescan verified shared subtrees.
The first time, every Tree is verified; afterwards only new Merkle nodes are processed.
Blob bodies are not read when building the membership index. The index persists across SQLite reopens.
`ListDir` also registers membership, as do the Trees / Blobs exposed in Merge / Rebase / Land conflict responses.

The Object RPC itself does not take paths or sparse settings.
Clients walk only the child Trees needed for the root and their profile, and request only the target Blobs.
A Tree whose membership has not yet been registered through `GetRootTree` or similar cannot be fetched even if its ID is known.
A batch mixing IDs from another repository is rejected before any body is sent.

## Limits and Stopping

One request has at most 128 IDs, the protobuf body is at most 16 KiB, and duplicate IDs are rejected.
An empty batch yields an empty stream.
The existing limit for decompressed objects is 256 MiB, and the delivery limit for compressed frames is 257 MiB.
The whole batch is not read into memory; one object and a send queue of at most 2 chunks are held at a time.
When the receiver stalls, backpressure applies, and after a disconnect the next object is not read.
On an error mid-delivery, complete objects already received can be reused,
but partial objects without termination cannot.

```sh
cargo test -p kasu-server --test objects --locked
cargo test -p kasu-store --test tree_membership --locked
```

These verify sparse requests over real gRPC, byte equality with stored frames, chunk boundaries, authorization and batch limits,
disconnects, store corruption / missing data, conflicting Trees, reopening, and skipping shared subtrees.
Real-network E2E for the [working copy / CLI / notify](native-cli.md) has also been added.
The shared daemon / cache hierarchy is remaining work for M9.

## Directory Read ACLs

An account with [read_directories](directory-read-acl.md) walks the canonical read view obtained via GetRootTree.
Objects and GetBlobMetadata authorize only IDs registered in the index for that repository / read path set.
They never reuse the repository-wide membership index or another scope's index.
Even when `KASU_OBJECT_TOKEN_FILE` is enabled, a scoped account can connect directly to the server with its own PAT and is subject to scope checks.
Using an unrestricted PAT in place of a hop token is rejected. Delegation through a shared cache that preserves the user's scope is not implemented.
