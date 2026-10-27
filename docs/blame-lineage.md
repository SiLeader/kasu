# Line Tracking Across Renames / Copies

English | [日本語](blame-lineage_ja.md)

Blame follows paths back before renames and returns the Changeset, original path, and original line number for each line.
Moves within the same file are tracked with `--follow-moves`, and line fragments or whole-file copies from other files with
`--follow-copies`. Copy tracking also enables move tracking. Both are false by default.

```sh
kasu blame path/to/file
kasu blame --follow-moves path/to/file
kasu blame --follow-copies path/to/copied-file
```

gRPC and HTTP JSON `/v1/blame` accept the same options.
The existing protobuf fields 1–4 are unchanged, and `follow_moves` is added as bool field 5.
`follow_copies` is field 4. Older clients that omit them get tracking options set to false, as before.
PathRequest is shared with ReadFile / ListDir, but `follow_copies` / `follow_moves` apply only to Blame.

## Derived Data

| Type | Key and contents |
|---|---|
| `PathIndex@1` | SubKey=BlobId. Maintains the sorted set of paths holding that content via the diff against the first parent. `under(prefix)` filters at path boundaries |
| `CopyDetect@1` | SubKey=RepoPath. Compares all parents with the current tree and returns the parent Changeset, source path, rename/copy, and similarity |
| `Blame@2` | SubKey=RepoPath. References the parent Blame of the same path or of a detected rename source |
| `LineMatches@1` | SubKey=(RepoPath, copies). Chooses matching lines for the same path / rename and move / copy fragments from parent snapshots, storing the original parent, path, and line |
| `MovedBlame@1` | SubKey=RepoPath. Also tracks moves within the same file / rename source |
| `CopyBlame@2` | SubKey=RepoPath. In addition to the above, tracks whole-file and line-fragment copy sources |

All are Lazy. Normal Changeset creation does not build PathIndex / CopyDetect or add every blob to an index.
PathIndex backfills the first-parent history of the requested BlobId. It removes changed paths and adds Sets of the same BlobId,
so merges also follow the final diff from the first parent. Backfill time and capacity on large histories requesting many blobs are unevaluated.

Immutable records of Blame@1 / @2 and CopyBlame@1 are kept.
The new CopyBlame@2 / MovedBlame@1 use separate keys, and the meaning of normal Blame@2 and PathHistory is unchanged.
If the source is not in the parent snapshot at the time the copy was made, it is not linked to the same content in older history.
Recreating after deletion merely because the same blob existed in the past does not revive old attribution.

## Detection Rules

When each parent has a file / symlink at the same path, the normal line diff is used. Otherwise, a source is chosen by the following rules.

1. Search with PathIndex for parent paths with the same BlobId as the current one. Regular files and symlinks are compared as different kinds.
2. Deleted sources and added destinations are assigned one to one, within groups of the same content and kind, in path byte order.
   A destination assigned a deleted source is a rename; remaining exact matches are copies.
3. Regular files without an exact match compare deletion and addition candidates with a line diff.
   If `matched line bytes / max(old bytes, new bytes)` is 50% or more, it becomes a candidate.
4. Exact-match pairs are reserved first, then candidates are assigned one to one by descending similarity, then by destination / source byte order.
5. Attribution of unchanged lines is inherited from the parent Blame of the rename source, or of the exact-match source when copy tracking is on.
   For merges, all parents are examined; if a parent has the same blob, its attribution is adopted. Otherwise matching lines are inherited in parent order.

Content containing NUL and content over 8 MiB are excluded from similarity estimation. Exact matches can be detected by content ID.
Similarity candidate comparison is limited to 1,000 pairs per call; exceeding it returns an explicit error, and incomplete CopyDetect results are not stored.

CopyDetect itself still follows the v1 rules above. It is not identical to Git's internal byte-block similarity or its choice among ambiguous candidates.

## Moves / Copies of Line Fragments

LineMatches first inherits matching lines for the same path / rename source from all parents using the same line diff as before.
If the content exactly matches a parent's blob, all of that parent's lines are inherited and no fragment search is done.
For the remaining unassigned lines, it searches in the following order.

1. Find consecutive lines that match exactly anywhere in the regular file at the same path / rename source.
   Fragments with at least 20 ASCII alphanumeric characters in total are treated as moves.
2. When copies are enabled, find unassigned consecutive lines in other regular files of each parent snapshot.
   Both new and existing files are targets, and fragments need at least 40 ASCII alphanumeric characters in total.
3. Choose by more alphanumeric characters, more lines, parent order, path byte order, original line, then destination line.
   Candidates overlapping already chosen fragments are rescored per remaining consecutive run.
4. Only the parent Blame of the chosen source file becomes an extra dependency. If the source line's attribution references an even older move / copy,
   it is inherited down to that Changeset, path, and line number. New lines are attributed to the current Changeset.

Files that did not exist at the parent are never searched for in older snapshots.
Symlinks and content containing NUL are excluded from fragment search. The existing exact-match copy can still track them.
Fragment search is limited to 8 MiB and 100,000 lines, and candidates from other files to 1,000 in total across all parents.
Building line position indexes, candidate search, and extending matching lines are limited to 1,000,000 steps in total.
Exceeding line, candidate, or comparison limits is an explicit error, and partial LineMatches / Blame are not stored.
Search limits do not apply to root Changesets or files exactly matching a parent, which need no fragment search.

This serves the same purpose as [Git blame's `-M` / `-C`](https://git-scm.com/docs/git-blame),
but it is not specified to match Git in candidate search, scoring boundaries, short exact-match copies, or tie-breaking.
`--follow-copies` also searches unchanged parent files, covering a wider range than a single Git `-C`.

## Dependency Scheduler

A stage that evaluates `resolved_dependencies` after a type's normal dependencies are resolved has been added.
CopyDetect checks RootTree to choose PathIndex keys, and Blame checks CopyDetect to choose the parent's other path.
LineMatches also chooses content matches first and adds only the source parent Blames that actually contribute as extra dependencies.
Those dependencies are resolved by the same iterative scheduler. The Engine is never called recursively during computation.
Cycles are detected for both normal and extra dependencies, and final values are stored using the existing singleflight / put-if-absent.

## Verification

With Git 2.54.0 on macOS arm64 and Git 2.39.5 on Linux arm64 with pinned Rust 1.90,
line attribution is compared in 3 modes (normal / moves / copies), 24 cases in total, for pure renames, edited renames, copies, edits after copies,
moves of line blocks, fragment copies into existing / new files, and edits after fragment copies.
Default results are compared against normal `git blame --line-porcelain`, and with copies enabled, exact-match copy fixtures against
`git blame -C1 -C1 --line-porcelain`. Move fixtures use `-M` and fragment copy fixtures use `-M -C -C -C`;
the Git OIDs corresponding to Changesets, original line numbers, and original paths matched.
Results from Backend, real gRPC, and HTTP JSON also match.

Unit and property tests confirm non-UTF-8 sources, exact binary matches, second-parent renames, deletion and recreation,
one-to-one conflict resolution, 1,024 renames, cycles through extra dependencies, comparison limits, and agreement between PathIndex and RootTree.
Original line numbers of every line are also compared under random reorderings of moved line blocks, and second-parent fragment copies,
not treating short common lines as copies, candidate / comparison limits, and skipping search for large roots are verified.
The real CLI E2E also verifies that `blame --follow-copies` / `--follow-moves` requests go through the RPC.
rename / copy parity on real OSS repositories, large-scale search performance, and differences from the Git heuristics above remain.

The [comparison results and source hashes](simulations/blame-lineage/summary.json) are stored.

```sh
cargo test -p kasu-derived --test lineage --locked
KASU_CAPTURE_DIR=/tmp/kasu-lineage-evidence \
  cargo test -p kasu-server --test metadata --locked
docker build -f tools/git-compat/Dockerfile -t kasu:lineage-test .
```
