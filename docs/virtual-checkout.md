# Virtualized Checkout

English | [日本語](virtual-checkout_ja.md)

On Linux, `kasu mount` starts a working copy that stacks a read-only FUSE layer and a writable OverlayFS.
Blobs are not fetched until files are read, and only modified working files are materialized in the upper directory.
The existing `status` / `diff` / `commit` / `rebase` / `land` / `sparse` use the same `.kasu` working state.

## Startup

Requires the Linux FUSE device, `mount` / `umount`, and permission to mount OverlayFS (`CAP_SYS_ADMIN`).
Choose a local filesystem for storage that can be used as an OverlayFS upper.
The kernel refused to start with the upper on Docker's own OverlayFS.
Verification used both a separate Docker volume and tmpfs.

```sh
export KASU_TOKEN=dev-token
kasu mount main /work/project --backing /work/project-state \
  --server http://127.0.0.1:50051 --repo demo --sparse app
```

The process runs in the foreground. Do normal work in another shell.

```sh
cat /work/project/app/file
printf 'changed\n' > /work/project/app/file
kasu -C /work/project status
kasu -C /work/project diff
kasu -C /work/project commit -m 'edit from virtual checkout'
kasu -C /work/project rebase --onto main
kasu -C /work/project land
```

The same Objects endpoints as the normal CLI can be specified, such as `--objects unix:///path/to/node.sock`.
Metadata connects to the authoritative server, and Trees / Blobs to the given Objects endpoint.
The mountpoint and backing must be separate directories, and a new backing and the mountpoint must be empty.
Control directory paths must be UTF-8, must not contain spaces, newlines, backslashes, commas, or colons, and cannot end in a symlink.
Paths inside the repository keep their bytes and can be non-UTF-8.

Ctrl-C or SIGTERM unmounts the upper OverlayFS and then the lower FUSE.
If unmounting fails because of open files or similar, FUSE stays mounted and waits; close the handles and send the signal again.
After a forced kill, restart with the same arguments. It takes the backing lock, unmounts only its own stale mounts,
and recovers using the stored starting point and upper. It never switches to the latest main automatically on restart.

## Storage and Lazy Fetching

| In backing | Contents |
|---|---|
| `mount.json` | Initial Changeset, ref, repo, endpoints, sparse profile, mountpoint. The token is not stored |
| `mount.lock` | Exclusive lock for the mount process |
| `lower/` | Mountpoint of the read-only FUSE |
| `upper/` | Modified working files, whiteouts, and the normal CLI's `.kasu` |
| `work/` | OverlayFS work area |

Initialization fetches only Trees within the profile to enumerate file entries; blob contents are not fetched.
The new `Metadata.GetBlobMetadata` / HTTP JSON `/v1/blob-metadata` returns sizes and Git blob OIDs of IDs whose repository membership has been checked,
up to 128 at a time. It reads stored BlobMetadata, not blob contents.
That size is used for FUSE attributes, so `stat` / `readdir` never prefetch blobs.

Regular-file reads and symlink readlinks fetch content from the Objects RPC.
FUSE caches verified blobs in temporary files under the backing directory (up to 512 MiB and 1,024 entries, least-recently-used eviction). Reads copy only the requested byte range, including for blobs over 32 MiB. The cache is discarded on unmount and also reuses the kernel page cache.
Large blobs are read as single objects, with the existing maximum of 256 MiB.
Each Objects fetch has a 30-second deadline; failures and size mismatches return file I/O errors.
Gitlinks are shown as directories containing `.kasu-gitlink`, as in a normal checkout.

Reading alone never creates working files in upper.
Copies needed for write / chmod / rename and the like, deletion whiteouts, and new file creation are handled by OverlayFS.
The CLI's update journal and ref CAS / Land are the same implementation as a normal checkout.
When a file is overwritten while its original blob has not been fetched, only that original blob is fetched right before a text diff.

The current native `status` / `commit` read files within the profile to compare contents.
This fetches blobs and caches them in `.kasu/objects`, but unmodified working files are not copied to upper.
The FUSE read cache and the capacity limits of the existing daemon / shared cache can be used.
Capacity management for the CLI's whole `.kasu/objects`, metadata volume for huge profiles, and long-running page cache and failure evaluation remain.

## Verification

On a real kernel on Linux arm64 with pinned Rust 1.90, the following were confirmed.

- 0 blob requests through `stat` / `readdir`. The first regular-file read makes 1 request, and repeating the same read makes 0 more.
- Non-UTF-8 files, symlinks, executable mode, Gitlinks, and exclusion outside the profile.
- No working files exist in upper even after reads, and they are materialized only after writes.
- Additions, edits, deletions, normal CLI status / diff / commit, rebase / Land including upstream updates, and matching native contents.
- Unmodified working files are not copied to upper even after native content scans.
- Remounting after SIGKILL keeps files, whiteouts, and post-land state.
- A shutdown request with files in use keeps the mount, and shutting down after closing handles unmounts both layers.

Normal CLI regression tests on macOS / Linux and Metadata gRPC / HTTP JSON also pass.
The [results and source hashes](simulations/virtual-checkout/summary.json) and each log are stored.
CI's [virtual.yml](../.github/workflows/virtual.yml) has an explicit job for real mounts.
Results of runs on GitHub are unconfirmed.

```sh
docker build -f tools/virtual/Dockerfile -t kasu:virtual-test .
docker run --rm --network none --mount type=volume,destination=/tmp \
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  kasu:virtual-test
```

## macOS Research

On 2026-10-24, Apple's FSKit sample and `mount_nfs(8)` as shipped with macOS and in Apple's published source were examined.
A macOS `kasu mount` adapter is not implemented yet. The normal materialized checkout is available.

| Approach | APIs / requirements confirmed | Work needed on the kasu side |
|---|---|---|
| FSKit | User-space file system extension. The sample uses `FSUnaryFileSystem` / `FSVolume` and the `com.apple.developer.fskit.fsmodule` entitlement | An adapter between a Swift / Objective-C extension and the Rust Objects / Metadata clients, copy-on-write storage, and on-device verification of launching and enabling it as an app |
| NFS loopback | The standard client's TCP, `port` / `mountport`, timeout and retry settings | A localhost NFS server, stable file handles / directory cookies, writes / flush and locking, bounded termination on disconnect, and on-device verification of mount / unmount |

The first candidate is using an FSKit extension as a thin OS adapter. NFS loopback requires implementing a separate wire protocol
and a server for file operations, and the default waiting behavior on server failure would also need evaluation.
This is a design decision based on research; neither approach demonstrates a successful real kasu mount on macOS.

References: [Apple FSKit sample](https://developer.apple.com/documentation/fskit/building-a-passthrough-file-system),
[Apple's mount_nfs(8) source](https://github.com/apple-oss-distributions/NFS/blob/main/mount_nfs/mount_nfs.8),
[fuser 0.18 Filesystem API](https://docs.rs/fuser/0.18.0/fuser/trait.Filesystem.html).
