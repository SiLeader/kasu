English | [日本語](git-push_ja.md)

# Git Smart HTTP Writes

`git-receive-pack` is served on the same HTTP port as Metadata / upload-pack.
Authentication uses Basic (any user name + token) or Bearer, and authorization is per repository.
Authentication happens before ref advertisement and request parsing.

```sh
git -c http.extraHeader='Authorization: Bearer dev-token' \
  push http://127.0.0.1:50080/my-repo.git main
```

Initial and incremental pushes, and clone→commit→push with normal / depth=1 clones, were verified with Git 2.54.0 / go-git v5.19.3.
Annotated tags, nested tags, tags pointing to blobs, deletion, allowing / denying force, and partial rejection across multiple refs are also verified.
The server does not run Git commands and does not keep Git repositories or received packs around.
It validates the pack in memory and converts it to raw objects and native Changesets.

## Protocol and Validation

The receive advertisement is in v0 format and offers `report-status`, `side-band-64k`, `delete-refs`, `ofs-delta`,
`object-format=sha1`, and agent. Protocol v2 extensions for push are not provided.
`report-status-v2` and push options are not advertised and are rejected if requested.
`atomic` is advertised on backends that can store branches and Git-only refs in the same CAS / log transaction.
SQLite supports it.
Multiple refs without `--atomic` are individual CASes.
For policy rejections or CAS conflicts on a valid pack, other refs can still succeed.
Pack / connectivity errors reject every ref, returning `unpack` and `ng` for each ref.

```sh
git -c http.extraHeader='Authorization: Bearer dev-token' \
  push --atomic http://127.0.0.1:50080/my-repo.git main refs/tags/v1
```

Following [Git's atomic capability](https://git-scm.com/docs/protocol-capabilities#_atomic),
when requested, the old OIDs, policies, and final-tree ACLs of every command are checked, and then
creation / update / deletion of branches / tags and the log appends for both kinds are stored in one transaction.
If any command is rejected, conflicts, or sees its ref version change after the check, every command gets `ng`.
Immutable objects are imported first, so objects from a failed request may remain.
ABA races where a ref returns to the same OID are also rejected using the version obtained at check time.

The embedding API `GitRefStore::supports_atomic_publication()` is false by default.
Only supporting backends return true and guarantee the atomicity of `compare_native_and_git`.
Without support, it is not advertised, and atomic requests sent directly by clients are rejected before storing.
Push atomicity is a guarantee about the storage transaction.
There is no feature for multiple independent read RPCs to share the same snapshot.

`gix-pack` parses entries and deltas, checking the SHA-1 trailer, actual zlib inflated length, types,
ofs-delta entry boundaries, ref-delta bases, and final object sizes.
Forward ref-deltas and thin packs based on existing published objects are supported.
Thin pack bases and omitted dependencies are limited to objects reachable from this repository's published refs.
Merely knowing an OID in the unpublished index does not allow creating a ref while omitting it from the pack.

| Default limit | Value |
|---|---:|
| pack | 128 MiB |
| Object count | 100,000 |
| Per entry / final object | 64 MiB |
| Expansion (including delta instructions, results, and external bases) | 512 MiB |
| Delta depth | 64 |
| Ref commands / shallow declarations | 1,024 each |

The HTTP body is limited to the pack limit plus 2 MiB for commands. gzip Content-Encoding on received HTTP is not supported.
When embedding, limits can be set with `Backend::with_receive_limits`.
Shallow declarations are accepted only for existing published commits, and full connectivity up to the new refs is verified.
There is no feature for creating a shallow repository with missing parents on the server side.

Separately, [tree diffs during import](git-import.md#inputs-explicitly-rejected) are bounded to
100,000 visited entries (including directories) and 64 MiB of cumulative generated full-path bytes per diff.
These fixed limits also apply to SSH pushes and are not changed by `Backend::with_receive_limits`.
Shared subtrees count at each distinct path even when the pack contains very few unique objects.
Exceeding a tree diff limit during import rejects the entire push with an `unpack` error and `ng` for
every ref, before publishing any ref.

Refs are not moved until connectivity and type validation finish.
Objects are imported parents first, storing the diff against the first parent and the raw Git commits / trees / tags, preserving the original OIDs.
After checking the ref's old OID, the CAS uses the native ref version at that point.
Each success and its ref_log / git_ref_log are in the same transaction.
Immutable objects from rejected requests may remain, but Git wants are checked for reachability from published refs.

## Ref Policy and Final-tree ACL

By default, creating branches / tags, fast-forwarding branches, and changing every path are allowed.
Deleting refs, non-fast-forward branch updates, and replacing existing tags are forbidden.
Writable namespaces are limited to `refs/heads/` and `refs/tags/`.

Start the CLI with `KASU_WRITE_POLICY_FILE=/path/to/policy.json`.
Example:

```json
{
  "allowed_ref_patterns": ["refs/heads/main", "refs/heads/image-updates/*", "refs/tags/*"],
  "force_ref_patterns": ["refs/heads/image-updates/*"],
  "allowed_path_prefixes": ["app", "deploy"],
  "allow_create": true,
  "allow_delete": false
}
```

`*` matches any bytes including `/`, and `?` matches one byte.
When a pattern list is given, it takes precedence over the corresponding legacy `allowed_prefixes` / `force_prefixes`.
If the patterns are empty, the prefix lists are used. To reject every ref, specify `allowed_prefixes: []`.
Each rule can be a UTF-8 string or an array of byte values, and unknown configuration keys are startup errors.

Paths are matched at component boundaries: `app` allows `app/config` but not `application/config`.
An empty path prefix means every path, and an empty list means no paths may be changed.
New refs are evaluated against an empty tree, and deletions as a new empty tree.
Commit / tree tags are checked too, and blob tags are rejected while path restrictions apply.
What is checked is the final diff between the old ref's tree and the new ref's tree, including changes via a merge's second parent and mode changes.
Force permission cannot bypass path ACLs.
Native UpdateRef / Land use the same final-tree ACL.

## Rerunning and Remaining Work

```sh
cargo test -p kasu-git --test pack_input --test receive --locked
cargo test -p kasu-server --test push --locked
docker build -f tools/git-compat/Dockerfile -t kasu:atomic-test .
go -C tools/git-compat build -o /tmp/kasu-go-git-compat .
KASU_GO_GIT_PUSH_CLIENT=/tmp/kasu-go-git-compat \
  cargo test -p kasu-server --test push --locked -- --include-ignored
```

Setting `KASU_CAPTURE_DIR` saves pkt-lines. Git timestamps / PIDs and Go sideband bodies are omitted,
and HTTP authentication headers are not recorded.
The measured versions and fixtures are recorded in the [compatibility matrix](../crates/kasu-git/tests/fixtures/clients/matrix.json).
The cloned Git directory is checked with `fsck --strict`, and server ref OIDs and native contents are compared.
Missing objects, unpublished OIDs, stale refs, authentication errors, and arbitrary packet / pack bytes are also tested.
Atomic push is verified with Git 2.54.0 on macOS and in a Linux container with pinned Rust 1.90,
including mixed branch / tag success, all refs and logs unchanged on rejection, ACLs, branch / tag ABA races
caused by a separate SQLite connection just before storing, and advertisement and rejection on unsupported backends.
In storage, a single winner and consistent logs for mixed CAS over 16 independent connections are checked,
and injecting an SQL error into the last Git log write verifies that both kinds of refs / logs roll back.

[Durable webhooks from ref_log](webhooks.md) are implemented.
In the [kind E2E](gitops-e2e.md), real commits and pushes by Image Updater v1.4.0 / Flux Image Automation v1.2.5,
Deployment updates via webhook, matching native content, and strict fsck passed.
The M7 acceptance criteria are verified. A nightly CI job has been added, but results on GitHub are unconfirmed.
