use kasu_core::{Entry, RepoPath, Result, Tree, TreeEntry, TreeId};
use kasu_derived::{LineEdit, line_edits, lines};
use kasu_store::Repository;
use std::{collections::BTreeSet, ops::Range};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineConflict {
    /// Zero-based, half-open range in the base's newline-preserving lines.
    pub base: Range<usize>,
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextMerge {
    Clean(Vec<u8>),
    Conflicted(Vec<LineConflict>),
    BinaryConflict,
}

fn render(base: &[&[u8]], other: &[&[u8]], span: Range<usize>, edits: &[&LineEdit]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut position = span.start;
    for edit in edits {
        for line in &base[position..edit.before.start] {
            output.extend_from_slice(line);
        }
        for line in &other[edit.after.clone()] {
            output.extend_from_slice(line);
        }
        position = edit.before.end;
    }
    for line in &base[position..span.end] {
        output.extend_from_slice(line);
    }
    output
}

/// Merge newline-preserving byte lines. Conflicts contain bytes, never inline markers.
/// Changes touching the same boundary are grouped, including simultaneous inserts.
pub fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8]) -> TextMerge {
    if ours == theirs || theirs == base {
        return TextMerge::Clean(ours.to_vec());
    }
    if ours == base {
        return TextMerge::Clean(theirs.to_vec());
    }
    if [base, ours, theirs].iter().any(|bytes| bytes.contains(&0)) {
        return TextMerge::BinaryConflict;
    }
    let ours_edits = line_edits(base, ours);
    let theirs_edits = line_edits(base, theirs);
    let mut edits: Vec<_> = ours_edits
        .iter()
        .map(|e| (false, e))
        .chain(theirs_edits.iter().map(|e| (true, e)))
        .collect();
    edits.sort_by_key(|(side, e)| (e.before.start, e.before.end, *side));
    let base = lines(base);
    let ours = lines(ours);
    let theirs = lines(theirs);
    let mut output = Vec::new();
    let mut conflicts = Vec::new();
    let mut cursor = 0;
    let mut i = 0;
    while i < edits.len() {
        let start = edits[i].1.before.start;
        let mut end = edits[i].1.before.end;
        let mut j = i + 1;
        while j < edits.len() && edits[j].1.before.start <= end {
            end = end.max(edits[j].1.before.end);
            j += 1;
        }
        for line in &base[cursor..start] {
            output.extend_from_slice(line);
        }
        let a: Vec<_> = edits[i..j]
            .iter()
            .filter_map(|(side, e)| (!side).then_some(*e))
            .collect();
        let b: Vec<_> = edits[i..j]
            .iter()
            .filter_map(|(side, e)| side.then_some(*e))
            .collect();
        let a_bytes = render(&base, &ours, start..end, &a);
        let b_bytes = render(&base, &theirs, start..end, &b);
        if a.is_empty() {
            output.extend(b_bytes);
        } else if b.is_empty() || a_bytes == b_bytes {
            output.extend(a_bytes);
        } else {
            conflicts.push(LineConflict {
                base: start..end,
                ours: a_bytes,
                theirs: b_bytes,
            });
        }
        cursor = end;
        i = j;
    }
    for line in &base[cursor..] {
        output.extend_from_slice(line);
    }
    if conflicts.is_empty() {
        TextMerge::Clean(output)
    } else {
        TextMerge::Conflicted(conflicts)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictKind {
    Content,
    Binary,
    AddAdd,
    DeleteModify,
    FileDirectory,
    TypeChange,
    Symlink,
    Gitlink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeConflict {
    pub path: RepoPath,
    pub kind: ConflictKind,
    pub base: Option<TreeEntry>,
    pub ours: Option<TreeEntry>,
    pub theirs: Option<TreeEntry>,
    pub lines: Vec<LineConflict>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeMerge {
    /// Present only when all paths merged without conflict.
    pub tree: Option<TreeId>,
    pub conflicts: Vec<TreeConflict>,
}

enum Task {
    Visit {
        path: Vec<u8>,
        base: Option<TreeEntry>,
        ours: Option<TreeEntry>,
        theirs: Option<TreeEntry>,
    },
    Finish(Vec<kasu_core::BString>),
}

fn subtree(repo: &Repository<'_>, entry: &Option<TreeEntry>) -> Result<Tree> {
    match entry {
        Some(TreeEntry::SubTree(id)) => repo.tree(*id),
        _ => Ok(Tree::default()),
    }
}

/// Iterative three-way tree merge. Unchanged subtrees retain their IDs.
/// Symlink targets and Gitlinks are opaque; divergent changes require resolution.
pub fn merge_trees(
    repo: &Repository<'_>,
    base: TreeId,
    ours: TreeId,
    theirs: TreeId,
) -> Result<TreeMerge> {
    // Check even the fast paths rather than returning an unchecked ID.
    for id in [base, ours, theirs] {
        repo.tree(id)?;
    }
    let mut tasks = vec![Task::Visit {
        path: Vec::new(),
        base: Some(TreeEntry::SubTree(base)),
        ours: Some(TreeEntry::SubTree(ours)),
        theirs: Some(TreeEntry::SubTree(theirs)),
    }];
    let mut results: Vec<Option<TreeEntry>> = Vec::new();
    let mut conflicts = Vec::new();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Finish(names) => {
                let children = results.split_off(results.len() - names.len());
                let tree = Tree {
                    entries: names
                        .into_iter()
                        .zip(children)
                        .filter_map(|(name, child)| child.map(|child| (name, child)))
                        .collect(),
                };
                results.push(if tree.entries.is_empty() {
                    None
                } else {
                    Some(TreeEntry::SubTree(repo.put_tree(&tree)?))
                });
            }
            Task::Visit {
                path,
                base,
                ours,
                theirs,
            } => {
                if ours == theirs || theirs == base {
                    results.push(ours);
                    continue;
                }
                if ours == base {
                    results.push(theirs);
                    continue;
                }
                let directories = matches!(
                    (&ours, &theirs),
                    (Some(TreeEntry::SubTree(_)), Some(TreeEntry::SubTree(_)))
                ) || ([&base, &ours, &theirs]
                    .iter()
                    .all(|entry| matches!(entry, None | Some(TreeEntry::SubTree(_))))
                    && [&ours, &theirs]
                        .iter()
                        .any(|entry| matches!(entry, Some(TreeEntry::SubTree(_)))));
                if directories {
                    let b = subtree(repo, &base)?;
                    let a = subtree(repo, &ours)?;
                    let t = subtree(repo, &theirs)?;
                    let names: Vec<_> = b
                        .entries
                        .keys()
                        .chain(a.entries.keys())
                        .chain(t.entries.keys())
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    tasks.push(Task::Finish(names.clone()));
                    for name in names.into_iter().rev() {
                        let mut child = path.clone();
                        if !child.is_empty() {
                            child.push(b'/');
                        }
                        child.extend_from_slice(&name);
                        tasks.push(Task::Visit {
                            path: child,
                            base: b.entries.get(&name).cloned(),
                            ours: a.entries.get(&name).cloned(),
                            theirs: t.entries.get(&name).cloned(),
                        });
                    }
                    continue;
                }
                let mut line_conflicts = Vec::new();
                let kind = match (&base, &ours, &theirs) {
                    (_, Some(TreeEntry::SubTree(_)), _)
                    | (_, _, Some(TreeEntry::SubTree(_)))
                    | (Some(TreeEntry::SubTree(_)), _, _) => ConflictKind::FileDirectory,
                    (None, _, _) => ConflictKind::AddAdd,
                    (_, None, _) | (_, _, None) => ConflictKind::DeleteModify,
                    (
                        Some(TreeEntry::Leaf(Entry::File {
                            blob: b,
                            executable: bm,
                        })),
                        Some(TreeEntry::Leaf(Entry::File {
                            blob: a,
                            executable: am,
                        })),
                        Some(TreeEntry::Leaf(Entry::File {
                            blob: t,
                            executable: tm,
                        })),
                    ) => match merge_text(&repo.blob(*b)?, &repo.blob(*a)?, &repo.blob(*t)?) {
                        TextMerge::Clean(bytes) => {
                            let blob = repo.put_blob(&bytes)?;
                            let executable = if am == bm { *tm } else { *am };
                            results.push(Some(TreeEntry::Leaf(Entry::File { blob, executable })));
                            continue;
                        }
                        TextMerge::Conflicted(lines) => {
                            line_conflicts = lines;
                            ConflictKind::Content
                        }
                        TextMerge::BinaryConflict => ConflictKind::Binary,
                    },
                    (
                        Some(TreeEntry::Leaf(Entry::Symlink { .. })),
                        Some(TreeEntry::Leaf(Entry::Symlink { .. })),
                        Some(TreeEntry::Leaf(Entry::Symlink { .. })),
                    ) => ConflictKind::Symlink,
                    (
                        Some(TreeEntry::Leaf(Entry::Gitlink { .. })),
                        Some(TreeEntry::Leaf(Entry::Gitlink { .. })),
                        Some(TreeEntry::Leaf(Entry::Gitlink { .. })),
                    ) => ConflictKind::Gitlink,
                    _ => ConflictKind::TypeChange,
                };
                conflicts.push(TreeConflict {
                    path: RepoPath::new(path)?,
                    kind,
                    base,
                    ours: ours.clone(),
                    theirs,
                    lines: line_conflicts,
                });
                // Only an internal placeholder; no partial tree is returned.
                results.push(ours);
            }
        }
    }
    let tree = if conflicts.is_empty() {
        Some(match results.pop().flatten() {
            Some(TreeEntry::SubTree(id)) => id,
            None => repo.put_tree(&Tree::default())?,
            _ => unreachable!("root is a directory"),
        })
    } else {
        None
    };
    Ok(TreeMerge { tree, conflicts })
}
