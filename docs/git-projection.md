# Git Path Projections

English | [日本語](git-projection_ja.md)

A read-only Git repository rooted at a given directory is served from the same native store.
The server never runs `git subtree` or `git upload-pack`.

```sh
git clone http://git:dev-token@127.0.0.1:50080/my-repo/-/path/deploy.git
git clone --depth=1 http://git:dev-token@127.0.0.1:50080/my-repo/-/path/deploy.git
git clone --filter=blob:none http://git:dev-token@127.0.0.1:50080/my-repo/-/path/deploy.git
```

In this example, `deploy/manifest.yaml` becomes `manifest.yaml` in the clone.
The source's branch names are used, and updates only outside the directory do not change the projected branch's OID.
The form `my-repo.git/-/path/deploy.git` also works.
Non-UTF-8 paths are specified as hex bytes. For example, the directory `0xff` is
`my-repo/-/path-hex/ff.git`, and `deploy` is `my-repo/-/path-hex/6465706c6f79.git`.
Either way, the same repo / path yields the same projection. Empty, absolute, dot-component, and NUL paths are rejected.

The trailing `.git` in the URL is an optional suffix and is not part of the directory name.
For a directory named `app.git`, use `my-repo/-/path/app.git.git` or
`my-repo/-/path-hex/6170702e676974.git`. The same rule applies to HTTP and SSH.

## History Generation Rules v1

`ProjectionId` holds the repo and a byte-string path, and is used as the Derived Data SubKey and as the Git OID index namespace.
The existing key format for normal repositories is unchanged; path projections use a different type tag.
Original commits are not copied; `PathCommit` is derived iteratively by the following rules.

1. Extract the given subtree from each source Changeset's RootTree and synthesize a Git tree.
   If the path does not exist, or an intermediate or final component is a file / symlink / gitlink, the tree is empty.
2. List the projected tips of all source parents in the source's parent order, excluding parents with no history yet and removing duplicates.
3. Remove parents that are ancestors of another projected parent, using the projected Git DAG.
4. If there are no parents and the tree is empty, no projected commit is created.
5. If there is one parent with the same tree, that parent becomes the current projected tip.
6. Otherwise, create a new commit. A merge joining multiple independent projected parents is kept even if its tree equals the first parent's.
7. Deleting the directory is kept as a commit with an empty tree. When it is recreated, that deletion commit becomes the parent.

author / committer / message come from the source Changeset, and the `kasu-changeset` and
`kasu-projection-v1` headers are included. Nothing depends on time or the connecting client.
Trees are synthesized from native File / Symlink / Gitlink entries, so non-canonical Git modes or tree ordering in the original are normalized in projections.
Extra headers and signatures of original commits are not inherited. Messages are not edited by path.

This design follows the purpose of [Git subtree split](https://raw.githubusercontent.com/git/git/v2.54.0/contrib/subtree/git-subtree.adoc),
but it is not specified to produce the same OIDs or merge choices as Git subtree.
The v1 rules above and golden OIDs are fixed.

## Refs and Tags

| Source ref target | Projection |
|---|---|
| commit | The projected tip for that commit. The ref is omitted if the path has no history |
| tree | The given subtree of that tree. The ref is omitted if the directory does not exist |
| blob | The ref is omitted, as there is no directory to project |
| annotated tag / tag chain | The object line is retargeted to the tip above and the tag chain is rebuilt |
| symbolic ref | The source's target name is kept. An unborn HEAD is treated as unborn |

The original signature of a rewritten annotated tag is no longer valid, so everything from the PGP / SSH / X.509 signature start line recognized by Git onward is removed.
The start formats follow [Git 2.54.0's implementation](https://raw.githubusercontent.com/git/git/v2.54.0/gpg-interface.c).
Original signatures are not verified, and no new signature is added.

## Authorization and Transport

The existing per-repo Basic / Bearer authorization runs before ref advertisement and projection derivation.
The want / have index and reachability are separated per projection. Original commits, blobs outside the path, and commits of another projection
cannot be wanted merely because they exist in the source repo. Blobs / trees with the same content may have the same Git OID across projections.

v0 / v1 / v2 clone / fetch, shallow, and `blob:none` with follow-up promisor fetches use the same upload-pack.
receive-pack advertisements and pushes are rejected with HTTP 405. Writes go through the source repository.

[Projection-only accounts](projection-accounts.md) can read only projections of allowed directories.
A normal operator PAT has permission for the whole source repo. This is not a mechanism for hiding source authors, messages, or ref names.
Per-directory reads through the native API and per-account write ACLs are separate work.
The first derivation processes the source's full all-parent history, and results are reused under immutable keys. Initial time on large repositories and capacity for many projections have not been evaluated.

## Verification

Verified with Git 2.54.0 on macOS arm64 and Git 2.39.5 on Linux arm64 with pinned Rust 1.90.

- v0 / v1 / v2, each with full history, depth=1, and partial clone, plus strict fsck.
- OIDs do not change for updates only outside the path, and fetching an update inside the path updates the content.
- Retargeting annotated / nested tags, tree tags, omitting blob tags, and stripping original signature suffixes.
- Rejecting wants for original commits / blobs outside the path / commits of another projection, rejecting push, and HTTP clone of non-UTF-8 paths.
- Unit DAG tests for keeping independent merges, removing ancestor parents, deletion and recreation, replacement by a file, and cache reuse.
- `git index-pack --strict` / `git fsck --strict` on generated packs, and golden deterministic OIDs.

See the [compatibility matrix](../crates/kasu-git/tests/fixtures/clients/matrix.json) for traffic records,
and the [verification results](../crates/kasu-git/tests/fixtures/clients/path-projection-summary.json) for source / traffic hashes and scope.
The [kind E2E](gitops-e2e.md#path-projection-urls) with path projection URLs configured in ArgoCD v3.5.6 / Flux v2.9.6 also passed.
Across three stages — initial, updates only outside the projection, and updates inside it — both tools' SHAs / ConfigMaps, refresh acceptance,
and subtree equality / strict fsck of real clones / fetches were confirmed.
The [traffic and sync results](../crates/kasu-git/tests/fixtures/clients/path-projection-kind/gitops-kind-summary.json) are stored.

```sh
cargo test -p kasu-git --test path_projection --locked
KASU_CAPTURE_DIR=/tmp/kasu-path-traces \
  cargo test -p kasu-server --test git_http path_projection --locked
docker build -f tools/git-compat/Dockerfile -t kasu:path-projection-test .
```
