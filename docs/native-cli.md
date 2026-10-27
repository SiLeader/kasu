# Native CLI and Working Copies

English | [日本語](native-cli_ja.md)

`kasu-cli` provides the `kasu` command. It targets Unix filesystems on macOS / Linux.

```sh
cargo build -p kasu-cli --locked
KASU_BIN="$(pwd)/target/debug/kasu"
export KASU_SERVER=http://127.0.0.1:50051
export KASU_REPO=my-repo
export KASU_TOKEN=dev-token
export KASU_AUTHOR_NAME='Your Name'
export KASU_AUTHOR_EMAIL='you@example.com'
"$KASU_BIN" checkout main work --sparse app --sparse lib
cd work
"$KASU_BIN" status
"$KASU_BIN" diff
"$KASU_BIN" commit -m 'Update configuration'
"$KASU_BIN" rebase --onto main
"$KASU_BIN" land
```

After adding the binary to PATH, it can be run as `kasu`.
The server is a gRPC URL. HTTPS is verified with WebPKI trust roots.
The token is passed through the environment variable or `--token` and is never stored in the working copy.
Name and email can also be given with `--author-name` / `--author-email`.
`-C <dir>` specifies the working copy to operate on, and commands can also run from anywhere beneath it.
Path arguments are relative to the repository root.

## Commands

| Command | Behavior |
|---|---|
| `checkout <ref or Changeset ID> <new directory>` | Resolves the base and creates the working copy, fetching only the needed Trees / Blobs |
| `clone <server> <repo> [directory]` | Checks out the repository's default branch. The directory defaults to the last segment of the name |
| `repo list [--prefix P]` / `repo create <name> [--default-branch B]` / `repo show <name>` | Lists, creates, and shows repositories via the [Admin API](multi-repository.md#admin-api) |
| `status` / `diff` | Compares real files at startup. Detects regular files, executable bits, symlinks, deletions, and additions |
| `status --watch` | Watches for changes with notify's FSEvents / inotify and reruns the full state comparison |
| `commit -m <message>` | Checks Blob existence / streams uploads, and creates a Changeset on the server |
| `rebase --onto <ref or ID>` | Rebases on the server if there is a draft; otherwise updates the base |
| `land [--ref <branch>]` | Stores a durable key and Lands. After success, updates to the returned Changeset |
| `sparse add <path>` / `sparse remove <path>` | Updates include / exclude and updates real files |
| `log [--path <path>] [--limit N]` | Queries history on the server |
| `blame [--follow-moves] [--follow-copies] <path>` | Gets Blame tracked back before renames. Explicitly tracks moves and whole / line-fragment copies from other files ([rules](blame-lineage.md)) |
| `show [ref or ID] [path]` | Gets a Changeset as JSON, or the content of a given file |

`--json` requests structured output for status / diff / log / Land / file show.
Path bytes are kept as JSON numeric arrays. Normal status output escapes control characters and
non-ASCII bytes for display.

Unlanded commits are updated as a single draft. From the second commit on,
the cumulative diff against the original parent is queried on the server and a new Changeset combining it with the current changes is created.
Changes from earlier commits are kept, and undoing additions is supported too. No local history is computed or stored.
Past Changesets that were created remain in the server's immutable store.

`rebase --onto` with a Changeset ID keeps the existing Land target.
A working copy newly created by `checkout` / `mount` from an ID has no Land target,
so specify it explicitly with `land --ref <branch>`. A rebase with a ref makes that ref the Land target.

## Sparse Profiles and Storage Format

Omitting `--sparse` selects everything. Repeating it selects only those paths.
`sparse remove app/tests` excludes a child subtree, and this also applies to files added later.
Afterwards, `sparse add app/tests/small` overrides the exclusion with a more specific include.
`.` is the whole root.

`.kasu/state.json` stores the current base, draft state, ref, profile, and per-path entries with
mtime / size / inode. To avoid missed changes, the current status implementation also hashes actual contents.
The cache `.kasu/objects` stores [Object RPC](object-api.md) zstd frames without conversion,
verifying chunk order, termination, decompression limits, and BLAKE3 on receipt.
The client never requests Trees / Blobs outside the profile.

The global `--objects <URL>` / `KASU_OBJECTS_ENDPOINT` specifies the Object RPC endpoint.
checkout stores the chosen URL, and later runs can override the stored URL for that run only.
`unix:///absolute/path/node.sock` connects to the [daemon / cache hierarchy](cache-hierarchy.md).
A new checkout without this setting connects directly to the server.

Symlinks are not followed; their target bytes are stored.
Existing Gitlinks are represented as a directory with a 40-character SHA in `<path>/.kasu-gitlink`,
and edits to the SHA can be committed. Submodule contents are not fetched.
Non-UTF-8 names that the macOS filesystem cannot represent are errors.
RPC and Linux working copies preserve bytes.

## File Protection, Conflicts, and Resends

checkout never overwrites an existing destination and publishes only after a temporary directory is complete.
Updates take an exclusive lock and refuse to conflict with uncommitted changes or untracked files.
Parent directories are walked from descriptors with `NOFOLLOW`, preventing writes through symlinks.
`.kasu` and `.git` are reserved at every level, including alternate spellings with case differences involving Unicode
and invisible characters that HFS+ ignores.
checkout and updates refuse repository content that would place administrative configuration or Git hooks,
or impersonate a nested working copy.
`.gitignore`, `.gitmodules`, and `.kasu-gitlink` are not part of this reservation.

Multi-file updates use staging / backup and an fsynced journal.
After an interrupted run, the next start rolls back to the old state, or commits the new state if the state file had been saved.
Files the user edited after the interruption are not overwritten; the original backup and journal are kept and an error is reported.
In that case, preserve `.kasu/transaction/backup/` and the current files before resolving the conflict.

Rebase / Land conflicts are saved to `.kasu/conflicts.json`, keeping the current files and base.
Fix the contents, commit, and rerun rebase / land.
On a lost Land response, the key is kept and the next `land` resends the same request.
`Retry` may also include a lost response after a successful store, so the same key is kept to check the result.
Creating another commit / rebase while the result is unknown is refused.
If you edited while waiting for the response, save those edits elsewhere, restore the working copy to clean,
resend the original Land, and then bring back the edits.

## Verification

```sh
cargo test -p kasu-cli --locked
docker build -f tools/cli/Dockerfile -t kasu:cli-test .
```

Verified on 2026-10-23 on macOS arm64 and in a Linux arm64 container with pinned Rust 1.90.
Over the real CLI / gRPC, the tests check the set of requested Trees / Blobs, that subtrees added after exclusion are not requested,
Blob upload, cumulative commits, file / directory conversion, rebase, Land, conflicts and resends,
notifications, Gitlinks, non-UTF-8 paths, and protection of untracked files and symlink parents.
Rollback on interruption / cleanup after commit, and user edits in between, are also verified.

On Linux, `kasu mount` for the [virtual checkout](virtual-checkout.md) is also available.
It uses lazy fetching of file contents and OverlayFS copy-on-write, and can be edited and landed with the normal CLI commands.

## Accounts with a Read Scope

For a PAT with [read_directories](directory-read-acl.md), the server returns Trees limited to the allowed scope even without a sparse profile.
Point the Metadata and Objects endpoints at the authoritative server.
commit / Land use the original Changeset as parent and keep out-of-scope files on the server.
Adding files outside the allowed scope is rejected. When changing the authenticated principal or read scope, prepare a new working copy.
There is no feature to erase already downloaded local content when permissions change.
