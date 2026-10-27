# Native Directory Read ACLs

English | [日本語](directory-read-acl_ja.md)

When `read_directories` is set on a repository account, native Metadata / Objects return
a read view of the allowed paths. The CLI can check out from that view, and commits / Lands within the allowed scope
use the original Changeset as parent, so files outside the scope are preserved.

```json
{
  "accounts": [
    {
      "name": "deploy-user",
      "token_file": "secrets/deploy-user.pat",
      "read_directories": ["deploy", "charts/team-a"],
      "write_policy": {
        "allowed_path_prefixes": ["deploy"],
        "allowed_ref_patterns": ["refs/heads/main"],
        "allow_create": false
      }
    }
  ]
}
```

Load this JSON through `KASU_ACCOUNTS_FILE` of the [repository account configuration](repository-accounts.md).
For read-only access, omit `write_policy`.
If both `read_directories` and `read_directories_hex` are omitted or `null`, the whole repository is readable as before.
When given, there must be 1–128 entries in total, each path up to 4096 bytes. An empty array is a startup error.
`read_directories_hex: ["ff"]` allows the non-UTF-8 path `0xff`.
Excluding duplicates and rules under an already allowed parent, permissions are identified by the normalized set of paths.
Allowing `app` does not allow reading `application`.

## What Metadata Shows

Changeset IDs, parents, author / committer, messages, refs, the existence of history, and ancestry / merge-base are
published as repository-wide metadata. This is not a feature for censoring human-written strings in commit messages and the like.
File contents, out-of-scope paths in change lists, out-of-scope tree entries, and raw Git provenance are not published.

| RPC | Response for a restricted account |
|---|---|
| GetChangeset / Log | Changes under allowed paths only, raw_git_commit omitted, read_filtered=true |
| GetRootTree | A canonical TreeId containing only allowed files and their ancestor directories, read_filtered=true |
| ListDir | Enumerated from the read view. Ancestors up to allowed paths can be traversed; out-of-scope siblings are not returned |
| ReadFile | Contents of allowed paths only. Out-of-scope paths get PermissionDenied |
| Diff | When paths are omitted, filtered to allowed paths; out-of-scope contents are not diffed |
| Log(path) / Diff(paths) | PermissionDenied unless each explicit path is within the allowed scope |
| Blame | The target path and every returned origin must be in scope. If a copy / move origin is out of scope, the whole request is rejected |
| GetBlobMetadata | Only Blob IDs published within the same read scope |
| ResolveRef / ListRefs / IsAncestor / MergeBase / FindChangesetByGitOid | Keep the original repository's IDs and history relations |

`GetChangeset.read_filtered` indicates that the response is not a complete representation of the original object.
`id` references the original Changeset, and the original canonical bytes must not be reconstructed or verified from a filtered response.
The original Changesets and the existing IDs / canonical codec are unchanged.
`RootTreeResponse.id` is the correct content ID of the view itself. Only boundary nodes leading to allowed paths are rebuilt,
and wholly allowed subtrees share their original IDs. Tree updates only outside the scope do not change the view's ID.

HTTP JSON and gRPC use the same logic. The added `read_filtered` is an additional protobuf field,
false for unrestricted accounts and in existing responses.

## Objects RPC and Fetch Order

The existing fetch order `ResolveRef` → `GetRootTree` → needed Trees / Blobs is used.
Allowed Blobs returned by ReadFile and change lists, and rewrite conflict products within allowed paths, are also registered in that read scope.
Objects requests and zstd frames keep their existing formats.

Permission indexes are separated per repository and normalized read path set.
Trees / Blobs that another user registered for the whole repository, or known original root IDs, cannot be fetched as is.
If identical content also exists under an allowed path, that content ID can be fetched.
Batches check every ID before delivering, so no frames are streamed from a batch containing a rejected ID.

Indexes and the view root cache are stored as immutable KV.
Parent nodes are registered after their children are verified, and the root cache is stored after the index is built, so everything can be recomputed after an interruption.
The same path set can be reused across restarts and PAT changes.
Narrowing permissions yields a different namespace, and IDs from old permissions are not reused automatically.
Content already published under past allowed paths with the same rules can still be fetched as history.
This is not a mechanism for erasing content a client has already downloaded.

Even in configurations using `KASU_OBJECT_TOKEN_FILE`, a scoped account can connect directly to the server's
Objects RPC with its own PAT. Each request is subject to that scope.
Replacing the hop token with an unrestricted operator / repository account PAT is still rejected.
The hop token is a cache credential that reads the whole repository.

The existing shared cache / daemon protocol does not delegate a user's read scope.
Restricted users should point their Objects endpoint at the authoritative server.
Handing over cache credentials means handing over the permissions configured for that cache.
A shared cache that preserves per-user scope, GC / capacity limits for durable indexes, and capacity evaluation for large path sets remain.

## Writer and Git

The effective path scope of a writer with a read scope is the range satisfying read_directories, the account's write policy,
and the server-wide write policy all together.
Explicit changes in CreateChangeset are limited to the read scope, and Blob references also require membership in the scope.
This rejects requests that move a guessed unpublished Blob ID under an allowed path to read its contents.
UploadBlobs existence checks also happen only within the scope, and unallowed content requires the actual bytes to match the ID.

When a Merge / Rebase / Land conflict occurs outside the scope, PermissionDenied is returned without out-of-scope entries or contents.
The final-tree ACL at publication is checked as before.
Land idempotent results are also separated per read scope, so changing permissions never refetches an old conflict response.
If only the PAT changes, keeping the account name and path set allows refetching existing results.

A normal Git URL contains the whole repository, so both reads and pushes are rejected for this account.
Reading [Git path projections](git-projection.md) rooted at allowed directories is available.
Pushes to projections are rejected. Git OID contents are never partially hidden and returned under the same ID.

```sh
KASU_TOKEN=deploy-user-pat kasu checkout main deploy-work \
  --server http://127.0.0.1:50051 --repo demo
# Even without --sparse, the server returns a tree limited to the allowed scope.
```

## Verification

Real Metadata / Writer / Objects RPC, HTTP JSON, and the CLI were verified on macOS arm64 and Linux arm64 with pinned Rust 1.90.
This includes filtering at root / partial ancestors / non-UTF-8, rejecting known original Trees / Blobs, pre-authorizing every ID in a batch,
rejecting copies and upload existence checks of unpublished Blobs, out-of-scope conflicts, and index separation on permission changes.
With 2 real server processes, PAT updates, and restarts, views, permission indexes, and Land results were refetched.
The real CLI did checkout→commit→Land without a sparse profile, confirming no unpublished Blobs were fetched and contents were preserved on the server.

```sh
cargo test -p kasu-server --test accounts --locked
cargo test -p kasu-cli --test e2e --locked
docker build -f tools/ssh/Dockerfile -t kasu:read-acl-test .
docker run --rm --network none --entrypoint /out/accounts-test kasu:read-acl-test
docker run --rm --network none --entrypoint /out/cli-e2e-test kasu:read-acl-test --skip virtual_mount
```

The [verification logs and source hashes](../crates/kasu-git/tests/fixtures/clients/read-acl/) are stored.
Native CLI tests have also been added to the [Git SSH CI](../.github/workflows/ssh.yml). Results of runs on GitHub are unconfirmed.
A security audit and evaluation of production permission design and key management are outside the scope of this work.
