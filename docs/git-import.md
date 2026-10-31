English | [日本語](git-import_ja.md)

# Git Import and OID Preservation

```sh
cargo run -p kasu-ingest -- <Git repository> <storage directory> <repository name>
```

An unregistered repository is created with `--create` before importing (`kasu-ingest --create <Git repository> <storage directory> team-a/infra`).
An unregistered name without `--create` is an error; only when the registry is empty is the name registered as a legacy key
([multiple repositories](multi-repository.md#startup-and-migration)). Imports into different repositories can run concurrently.
For a newly registered repository, a symbolic source HEAD initializes `default_branch`, including when the branch is unborn.
Reimporting does not replace an existing repository's configured default branch.

Git bare / working repositories are read with gix, and parents and dependent objects are processed first and converted into Changesets.
Data is stored in `meta.db` (SQLite) and `objects/` (a zstd content-addressed store).
`RUST_LOG` sets the tracing log level.

Branches are converted to native refs named `refs/heads/*`. Tags and HEAD are stored as Git-only refs.
The server advertises the registry's `default_branch` as its HEAD; the stored source HEAD remains available for lossless export.
For a detached source HEAD, choose the hosted default branch explicitly through the Admin API.
During import only immutable data is written; after connectivity, type, and tree reproducibility are verified, every ref and the corresponding logs
are published in a single transaction. On a CAS conflict, no ref is updated and an error is returned.
Repeating the same import is idempotent; if the refs are unchanged, the ref_log does not grow.

## Lossless Representation

- Raw bytes of commits and tags are kept. Signatures, extra headers, and message line endings are not normalized.
- Raw trees are also kept per original OID. Multiple non-canonical trees corresponding to the same native Tree can coexist.
- file / executable / symlink / gitlink and non-UTF-8 paths, names, emails, and messages are supported.
- Blobs record their size and Git OID together.
- Index keys include the repository. Even if the same raw object exists in another repository, it cannot be found through this index.
- When a native commit comes back through Git, the existing Changeset ID is reused.

Git synthesis of native Changesets is fixed as v1. All parents are written in order, the `kasu-changeset` header is always included,
and the message is emitted as is. `GitTree` and `GitCommit` are Derived Data with ProjectionId as SubKey.
Conversion results of shared tree nodes are stored too, and only changed nodes are synthesized.

## Inputs Explicitly Rejected

Currently SHA-1 repositories are supported. SHA-256, shallow history, missing / mistyped dependencies, duplicate tree names,
names that cannot be represented as paths, empty directories that FileChange cannot reproduce, and symbolic refs other than HEAD are errors.
Each object is checked against a 256 MiB limit both at the header stage and after reading.
Submodule contents are not fetched; only the gitlink commit OID is kept.

Each tree diff, including first-parent diffs during import, has fixed limits of 100,000 visited entries
and 64 MiB of cumulative generated full-path bytes. Entries include directories and unchanged names
in visited tree pairs; identical subtrees are skipped without descending. Shared subtrees are counted
again at each distinct path, and generated directory prefixes count toward the path-byte limit.
Exceeding either limit is an error, not a truncated diff, and aborts import before ref publication.
These bounds prevent a small shared tree graph from expanding into unbounded file changes.

## Verification

Normal tests generate fixtures on the fly containing non-UTF-8, submodules, symlinks, executables, 2-parent merges, tag→tag, and tree / blob tags.
They import from a source packed by Git and check every raw byte string.
The non-delta pack from `write_pack` is imported with `git index-pack --stdin --strict`, and after restoring refs,
`git fsck --strict` and clone are run. Non-canonical trees are checked against raw OIDs and contents in a separate test.
There is no claim that the non-canonical input itself passes Git's strict checks.

Real OSS verification uses [libgit2/TestGitRepository](https://github.com/libgit2/TestGitRepository).
All 7 refs (21 commits, 70 objects) pinned in `crates/kasu-ingest/tests/fixtures/oss-refs.txt` are checked in full.
This repository contains an octopus merge and tags pointing to commits / trees / blobs.
See below for additional verification with a gitlink fixture from a pinned libgit2 release.
A pinned GitLab upstream input that combines non-UTF-8 paths, gitlinks, annotated tags, and merges has also been added, and round trips of all 183 OIDs are verified as described below.

```sh
KASU_OSS_REPO=/path/to/TestGitRepository.git cargo test \
  -p kasu-ingest --test roundtrip pinned_oss_repository --locked -- --ignored
```

Network-dependent tests are separated from normal tests. The nightly CI fetches the pinned OIDs and runs them.
Nightly CI runs on GitHub have not yet been confirmed.

Imported repositories can be served with [Smart HTTP reads](git-read.md).
Received packs and writes are recorded in [Git push](git-push.md), and syncing with real controllers in [GitOps E2E](gitops-e2e.md).
Git synthesis of native commits explicitly rejects messages containing NUL and values that Git signature headers cannot represent.

## Selection Record for Additional OSS Inputs

On 2026-10-26, four upstream fixtures from libgit2 v1.9.0 (commit `338e6fb681369ff0537719095e22ce9dc602dbf0`)
were examined as Git objects.
`submodules.git` has 2 commits, 6 objects, and 1 gitlink, and passed strict fsck.
`binaryunicode` also passed, but neither had non-UTF-8 tree names.
`nasty` and `testrepo.git` fail strict fsck, so they are not used as is as acceptance evidence for valid input.
The [selection results](git-import-upstream-research.json) are a Git-side survey and are distinct from Kasu import verification.
The Kasu round-trip verification of the gitlink that followed this selection survey is complete, as described below.
After this initial survey, a pinned input containing non-UTF-8 paths was selected from the GitLab upstream branch below, and its Kasu round-trip verification is complete.
Additionally, the names in 2,308 libgit2 loose objects and in 7 JGit packs / 42 trees were examined, but no matching non-UTF-8 names were found.
242 gitoxide fixture scripts were also read for selection, but this is not a survey proving absence across the whole archive.
In go-git's pinned corpus, 22 small SHA-1 packs with 3,704 trees in total were examined, with no matching names.
One large unfetched pack and one pack with missing bases were out of scope. Each pinned commit and survey scope is recorded in the same selection results JSON.


## Round Trip of the Pinned Upstream Gitlink Fixture

This uses `tests/resources/submodules.git` from libgit2's pinned commit
[`338e6fb681369ff0537719095e22ce9dc602dbf0`](https://github.com/libgit2/libgit2/tree/338e6fb681369ff0537719095e22ce9dc602dbf0/tests/resources/submodules.git).
It is an upstream test input and is not representative of large real product histories.
It has 6 reachable objects in total (2 commits, 2 trees, 2 blobs) and 1 gitlink.
The 3 dangling blobs in the original ODB are excluded from the history under test.

```sh
python3 tools/oss-fixtures/prepare.py --output /tmp/kasu-upstream-submodule.git
KASU_OSS_SUBMODULE_REPO=/tmp/kasu-upstream-submodule.git \
  cargo test -p kasu-ingest --test roundtrip pinned_upstream_submodule --locked -- --ignored
```

Specify an output directory that does not exist yet.
The preparation tool fetches the pinned commit and reads only HEAD / refs / objects from the archive's regular files.
It does not bring in upstream config / hooks / alternates or worktree symlinks.
It bare-clones the reachable history with Git's local upload-pack and checks all 6 OIDs, the pinned ref values, and strict fsck.
With `--source /path/to/libgit2.git`, a local source containing the pinned commit and needed blobs can also be used.
Both acquisition methods were confirmed to produce the same manifest.

The Kasu test checks each object's raw bytes, strict index-pack of the generated pack, ref / tag peeling,
symbolic HEAD, and strict fsck and all OIDs of the export / bare clone.
It also checks that `testrepo` at the native root is an `Entry::Gitlink` with the original target `a65fedf39aefe402d3bb6e24df4d4f5fe4547750`,
and that the target commit is not imported.
No feature for cloning / tracking submodule contents is added.

11 tests, including the existing TestGitRepository and the GitLab input in the next section, passed on macOS arm64 / Git 2.54.0,
and 10 tests passed on Linux arm64 / Git 2.39.5 with pinned Rust 1.90.
On Linux the fixture is mounted read-only and copied to a temporary area in the container to keep Git's ownership checks.
The network is disabled while running.

```sh
python3 tools/oss-fixtures/prepare_byte_paths.py --output /tmp/kasu-upstream-byte-paths.git
docker build -f tools/oss-fixtures/Dockerfile -t kasu:oss-fixtures-test .
docker run --rm --network none --user "$(id -u):$(id -g)" \
  --mount type=bind,source=/tmp/kasu-upstream-submodule.git,target=/fixtures/submodules.git,readonly \
  --mount type=bind,source=/tmp/kasu-upstream-byte-paths.git,target=/fixtures/byte-paths.git,readonly \
  kasu:oss-fixtures-test
```

The [input manifest, run logs, and source hashes](git-import-upstream-submodule/summary.json) and
[CI](../.github/workflows/oss-roundtrip.yml) have been added. Results of runs on GitHub are unconfirmed.
The acceptance input containing non-UTF-8 paths is the GitLab fixture in the next section.

## Round Trip of the Pinned Upstream Byte-Path Fixture

This uses the pinned tip
[`99e4853698007ed7e68826730ef5313d24841991`](https://gitlab.com/gitlab-org/gitlab-test/-/commit/99e4853698007ed7e68826730ef5313d24841991)
of `invalid-utf8-diff-paths` published by [GitLab.org/gitlab-test](https://gitlab.com/gitlab-org/gitlab-test), and
the pinned OID `f4e6814c3e4e7a0de82a9e7cd20c626cc963a2f8` of the annotated tag `v1.0.0`.
That is 183 objects in total: 50 commits with full parent history including 14 merges, 76 trees, 56 blobs, and 1 tag.
Every object reachable from these 2 pinned refs is in scope. Other upstream refs are out of scope.
This is an externally created test repository and is not representative of large production histories.

The path `hello\x80world` directly under the root is `68656c6c6f80776f726c64` in hex.
It cannot be converted to a UTF-8 string.

| Operation | Original commit | blob OID |
|---|---|---|
| Add | `d0888d297eadcd7a345427915c309413b1231e65` | `3742e48c1108ced3bf45ac633b34b65ac3f2af04` |
| Content update | `19950f03c765f7ac8723a73a0599764095f52fc0` | `ea20973903d3800ac659d503596f9105f0a7423f` |
| Delete | `99e4853698007ed7e68826730ef5313d24841991` | none |

```sh
python3 tools/oss-fixtures/prepare_byte_paths.py --output /tmp/kasu-byte-paths.git
KASU_OSS_BYTE_PATH_REPO=/tmp/kasu-byte-paths.git \
  cargo test -p kasu-ingest --test roundtrip pinned_gitlab --locked -- --ignored
```

Specify an output directory that does not exist. The preparation tool fetches the pinned commit / tag and checks
the [183 pinned OIDs and types](../crates/kasu-ingest/tests/fixtures/upstream-byte-path-objects.txt), the full reachable set,
ref / HEAD, strict fsck, the 14 merges, the 3 snapshots of the path above, and tag peeling before publishing the output.
There is no shallow boundary. It is handled as a bare repository, and no submodule or LFS external content is fetched.

With `--source /path/to/cached-repo.git`, the same 183 raw objects are read, checked against their original OIDs, and stored in a new bare repository.
The source's config / hooks / alternates / worktree are not carried into the new output.
It was confirmed that fetching from the publisher and preparing from a cached source produce the same manifest.

After the Kasu import, the following are checked.

- Raw bytes and OIDs of all 183 objects, strict index-pack, and strict fsck and all OIDs of the export / bare clone.
- Preservation of the branch, the annotated tag and its peel, and the symbolic HEAD.
- The native tree keeps the raw path and contents above, and Set / Delete point to the same path.
- Ordered equality of native parent lists and Git parent lists for all 50 commits, including the 14 merges.
- HEAD's `gitlab-grack`, `gitlab-shell`, and `six` become native Gitlinks with the original target OIDs, and the target commits are not imported.

11 tests on macOS arm64 / Git 2.54.0 and 10 tests on Linux arm64 / Git 2.39.5 with pinned Rust 1.90 passed.
On Linux, the Docker command in the previous section passes both inputs read-only and runs with the network disabled.
This is not a test of materializing a working tree with non-UTF-8 names on APFS.

Results and source hashes are stored in [summary.json](git-import-byte-paths/summary.json), the [input manifest](git-import-byte-paths/preparation.json),
the [macOS log](git-import-byte-paths/macos.log), the [Linux log](git-import-byte-paths/linux.log), and
[Linux build information](git-import-byte-paths/linux-build.log).
`tools/oss-fixtures/record_byte_paths.py` cross-checks matching preparation results, counts, test names, and Linux source hashes and image manifests.
It has also been added to the nightly workflow. Results of runs on GitHub are unconfirmed.
