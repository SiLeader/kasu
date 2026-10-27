# Native 3-way Merge Foundation

English | [日本語](native-merge_ja.md)

`kasu-ops::merge_text(base, ours, theirs)` and `merge_trees(repo, base, ours, theirs)` are
the foundation of M6. The tree operation uses the common base explicitly provided by the caller.
They are also used by the Changeset-level `Operations::merge` / `rebase` and the [Write API](write-api.md),
and by reapplication in [Land](land.md).

`merge_text` computes diffs over byte-string lines that keep their line endings.
It does not convert to UTF-8 and preserves whether there is a trailing newline.
When both sides make the same change, or only one side changes, that content is adopted.
Changes to separate lines are combined; overlapping edits and edits touching the same boundary are compared together.
On conflict, it returns the base's 0-based half-open line range and both sides' bytes for every conflict.
Inline conflict markers are not stored. Content containing NUL that both sides changed differently is a binary conflict.

`merge_trees` walks trees with an explicit stack and keeps the IDs of unchanged subtrees.
Regular files at the same path have their content and executable bit merged 3-way independently.
Directories where both sides added different child files are combined.
A deletion of the last old file by one side can also be combined with an independent new file added by the other side.
Symlink and gitlink contents are treated as opaque values, and different changes on both sides conflict.
No rename / copy inference is done.

`TreeConflict` holds the byte-string path, the conflict kind, the base / ours / theirs `TreeEntry`,
and the line ranges of content conflicts. Kinds are content, binary, add/add, delete/modify,
file/directory, type change, symlink, and gitlink.
If there is even one conflict, `TreeMerge.tree` is `None`.
Immutable blobs / trees from the tree operation may be stored, but no Changeset or ref is published.

## Changeset-level Operations

`merge(a, b, metadata)` returns the descendant if one side is an ancestor.
If they diverge, it 3-way merges the common base and both tips, and creates a Changeset with parents `[a, b]`.
If there is no common ancestor, the empty tree is the base.
Multiple best merge-bases are combined in ID order through virtual merges, each of which computes its own common base.
This also uses an explicit stack, and virtual Changesets are not added as parents of the final result.
If combining the bases itself conflicts, the `MergeBases` stage and the original list of best bases are returned.
Embedding markers into a conflicted virtual base and continuing with later operations is not adopted.

`rebase(cs, onto, committer)` reapplies one Changeset's diff against its first parent onto `onto`.
It keeps the author and message, replaces the committer, and sets the parents to `[onto]`.
A root Changeset is treated as a diff against the empty tree and a merge Changeset as a diff against its first parent; the latter ends up with one parent.
It is not an API for transplanting an entire history at once.
Neither operation updates refs. The API makes successful Changesets members of the given repository.

Verification:

```sh
cargo test -p kasu-ops --test merge --locked
cargo test -p kasu-ops --test rewrite --locked
```

144 positional boundary combinations of insertions, deletions, and replacements plus extra fixtures are compared against Git's `merge-file`.
Identity, one-sided changes, and left/right symmetry are checked with proptest; independent content / mode changes, delete vs. add,
file/directory, symlink, gitlink, binary, type change, and retention of every conflict are verified individually.
Changeset parents, signatures, and messages, fast-forward, reapplying root / merge Changesets,
criss-cross merges with 2 / 3 best bases, and conflicts within the bases themselves are also verified.
