# Git LFS Evaluation

English | [日本語](git-lfs_ja.md)

For the G6 LFS investigation, a setup that pairs Kasu with an external LFS service was verified with real clients.
Kasu stores LFS pointers losslessly as ordinary blobs, and the content is managed by a separate service.
clone / push work in this setup, but Kasu does not implement its own LFS endpoint or transparent native content fetching.
The policy in `docs/plan/README.md` that "tracking LFS contents is a non-goal" is kept.

## Storage and Retrieval

In the [Git LFS specification](https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md), Git objects store
pointers with a SHA-256 and size, and clean / smudge filters convert to and from the content.
LFS SHA-256 is an identifier separate from Git's SHA-1 and Kasu's BlobId.
Kasu's Git import, push, Metadata ReadFile, and Objects RPC handle the pointer bytes.
Replacing LFS content with native blobs would break Git OID round trips, so nothing is expanded automatically.

```text
Git client -- Git pack(pointer) --> Kasu native storage
           -- LFS upload/data --> external LFS service
Git client <-- Git pack(pointer) -- Kasu native storage
           <-- LFS download/data -- external LFS service
```

Either include `.gitattributes` and a root `.lfsconfig` in history, or configure `lfs.url` separately on clients.
Per the [Git LFS configuration specification](https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-config.adoc),
regular Git config takes precedence over `.lfsconfig`.

```sh
git lfs install
git lfs track '*.bin'
git config -f .lfsconfig lfs.url https://lfs.example.test/team/repo
git add .gitattributes .lfsconfig asset.bin
git commit -m 'Track external asset'
git push origin main
```

When importing an existing Git repository into Kasu, `kasu-ingest` moves only the pointers.
Keep the content in the original LFS service, or fetch it at the source and transfer it to the destination.
In this evaluation, `git lfs push --all origin` was run with the content present in the source.

Kasu's `repo.git/info/lfs/objects/batch` returns 404.
[Batch API](https://github.com/git-lfs/git-lfs/blob/main/docs/api/batch.md) uploads / downloads are
sent to the external service. Git connection credentials and the LFS service's credentials / authorization are configured separately.
The peer here was a loopback-only unauthenticated test peer; authentication, TLS, and production operation of an external service are unverified.
The Git SSH forced command does not accept `git-lfs-authenticate` / `git-lfs-transfer`.
Using LFS with an SSH remote also requires an explicit external HTTP(S) endpoint,
but this LFS E2E targets Git Smart HTTP, and the SSH + LFS combination is unverified.

## Real-traffic Results

Verified in a Linux arm64 container with pinned Rust 1.90, Git 2.39.5, and Git LFS 3.3.0
(Debian `3.3.0-1+deb12u2`), using a deterministic 2 MiB binary.

| Check | Result |
|---|---|
| native import → clone without LFS content | Checkout fails. The Git pointer history passes strict fsck |
| Upload existing content to external LFS | SHA-256 and all bytes match |
| full / depth1 / blob:none clone | All 3 cases fetch the content; content matches, Git fsck and LFS fsck pass |
| Change content and do a normal Git push | pre-push uploads the content, and Kasu publishes the new pointer |
| pull from a shallow client | Updates to the new content; content matches and both fscks pass |
| Metadata ReadFile | Matches the latest pointer byte for byte |
| Push a pointer without content, skipping pre-push | Kasu accepts it as Git history. It does not guarantee the external content exists |
| LFS service returning 503 | skip-smudge clone and Git fsck succeed; LFS pull fails |
| Kasu's LFS batch URL | 404. Kasu does not pretend to have built-in LFS |

For 2,097,152 bytes of content, the initial pointer is 132 bytes.
This compares Git blob and LFS data sizes; it is not a bandwidth benchmark including HTTP / pack compression, headers, or retries.
The external peer verifies the SHA-256 of every upload and records the 2 content uploads and the clone / pull downloads.
The [results, request records, and source hashes](../crates/kasu-git/tests/fixtures/clients/lfs/) are stored.
Because `.lfsconfig` contains a dynamic fixture port, Git commit OIDs change on every run.
The content SHA-256 and pointers are fixed.

## Decision and Constraints

External LFS is not required for GitOps manifest delivery or native metadata-centric operations.
For users with existing LFS repositories, lossless pointer storage and the use of an external service are provided.
This evaluation does not select an external LFS product or cloud service.

- The Git ref CAS and the external LFS upload are not a single transaction. A Git push rejected after upload can leave orphaned content.
- Git reachability and strict fsck do not guarantee LFS content exists. Content can be lost by skipping pre-push, creating pointers through the native API, or deleting external data.
- The native CLI's checkout / diff / merge handle pointers. There is no LFS locking, semantic merge of content, or transparent smudge.
- Kasu's path ACLs / Git projections are not automatically reflected in access control for external LFS content. Authorize separately on the external side.
- When a path projection excludes the root `.lfsconfig` / `.gitattributes`, the LFS configuration differs from a normal clone. This combination is unverified.
- External store GC, backup, capacity, billing, Range / resume, and very large file performance are not evaluated. Kasu backups alone cannot restore content.

Introducing built-in LFS would require designing not only Batch / Basic transfer but also repo / ref / projection authorization,
pointer consistency between native and Git, ordering of external uploads and ref publication, and retention / GC / backups.
There is currently no basis for adding that implementation, and the non-goal of tracking LFS contents is unchanged.

## Rerunning

```sh
docker build -f tools/lfs/Dockerfile -t kasu:lfs-test .
mkdir -p /tmp/kasu-lfs-evidence
docker run --rm --network none \
  --mount type=bind,source=/tmp/kasu-lfs-evidence,target=/evidence \
  kasu:lfs-test
python3 tools/lfs/record.py /tmp/kasu-lfs-evidence
```

`tools/lfs/run.py` starts the real `kasu-ingest` / `kasu-server` / `git-lfs`.
The in-memory LFS peer in a separate thread implements only the fixtures needed for batch and Basic upload / download;
it is not a production LFS server. It has process deadlines and shutdown handling, and makes no connections outside the container.
[CI](../.github/workflows/lfs.yml) runs the same steps. Results of runs on GitHub are unconfirmed.
