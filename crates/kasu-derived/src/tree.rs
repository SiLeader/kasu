use kasu_core::{BString, Entry, FileChange, RepoPath, Result, Tree, TreeEntry, TreeId, invalid};
use kasu_store::Repository;
use std::collections::BTreeMap;

const MAX_DIFF_ENTRIES: usize = 100_000;
const MAX_DIFF_PATH_BYTES: usize = 64 * 1024 * 1024;

pub fn lookup(repo: &Repository<'_>, root: TreeId, path: &RepoPath) -> Result<Option<TreeEntry>> {
    let mut node = root;
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        let tree = repo.tree(node)?;
        match tree.entries.get(part) {
            Some(TreeEntry::SubTree(id)) if parts.peek().is_some() => node = *id,
            Some(entry) if parts.peek().is_none() => return Ok(Some(entry.clone())),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn edit(
    repo: &Repository<'_>,
    root: TreeId,
    path: &RepoPath,
    value: Option<&Entry>,
) -> Result<TreeId> {
    let components: Vec<_> = path.components().collect();
    let mut ancestors = Vec::new();
    let mut tree = repo.tree(root)?;
    for component in &components[..components.len() - 1] {
        let child = match tree.entries.get(*component) {
            Some(TreeEntry::SubTree(id)) => repo.tree(*id)?,
            Some(TreeEntry::Leaf(_)) => return Err(invalid("file/directory collision")),
            None => Tree::default(),
        };
        ancestors.push((tree, BString::from(*component)));
        tree = child;
    }
    let name = BString::from(*components.last().unwrap());
    if matches!(tree.entries.get(&name), Some(TreeEntry::SubTree(_))) {
        return Err(invalid("file/directory collision"));
    }
    match value {
        Some(entry) => {
            tree.entries.insert(name, TreeEntry::Leaf(entry.clone()));
        }
        None => {
            tree.entries.remove(&name);
        }
    }
    while let Some((mut parent, name)) = ancestors.pop() {
        if tree.entries.is_empty() {
            parent.entries.remove(&name);
        } else {
            parent
                .entries
                .insert(name, TreeEntry::SubTree(repo.put_tree(&tree)?));
        }
        tree = parent;
    }
    repo.put_tree(&tree)
}

pub fn apply(repo: &Repository<'_>, root: TreeId, changes: &[FileChange]) -> Result<TreeId> {
    if changes.windows(2).any(|p| p[0].path() >= p[1].path()) {
        return Err(invalid("changes must be sorted and unique"));
    }
    // Validate deletes against the original first-parent tree, then remove before
    // inserting so a single changeset can replace a file with a directory or back.
    for change in changes {
        if let FileChange::Delete { path } = change
            && !matches!(lookup(repo, root, path)?, Some(TreeEntry::Leaf(_)))
        {
            return Err(invalid("delete target is not a file in the first parent"));
        }
    }
    let mut current = root;
    for change in changes {
        if let FileChange::Delete { path } = change {
            current = edit(repo, current, path, None)?;
        }
    }
    for change in changes {
        if let FileChange::Set { path, entry } = change {
            current = edit(repo, current, path, Some(entry))?;
        }
    }
    Ok(current)
}

pub fn flatten(repo: &Repository<'_>, root: TreeId) -> Result<BTreeMap<RepoPath, Entry>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(Vec::new(), root)];
    while let Some((prefix, id)) = stack.pop() {
        for (name, entry) in repo.tree(id)?.entries {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&name);
            match entry {
                TreeEntry::Leaf(entry) => {
                    out.insert(RepoPath::new(path)?, entry);
                }
                TreeEntry::SubTree(id) => stack.push((path, id)),
            }
        }
    }
    Ok(out)
}

pub fn diff(repo: &Repository<'_>, base: TreeId, head: TreeId) -> Result<Vec<FileChange>> {
    diff_with_budget(repo, base, head, MAX_DIFF_ENTRIES, MAX_DIFF_PATH_BYTES)
}

fn diff_with_budget(
    repo: &Repository<'_>,
    base: TreeId,
    head: TreeId,
    mut entries_left: usize,
    mut path_bytes_left: usize,
) -> Result<Vec<FileChange>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(Vec::new(), Some(base), Some(head))];
    while let Some((prefix, base, head)) = stack.pop() {
        if base == head {
            continue;
        }
        let base = base
            .map(|id| repo.tree(id))
            .transpose()?
            .unwrap_or_default();
        let head = head
            .map(|id| repo.tree(id))
            .transpose()?
            .unwrap_or_default();
        if base.entries.len().max(head.entries.len()) > entries_left {
            return Err(invalid("tree diff entry limit exceeded"));
        }
        let names: std::collections::BTreeSet<_> =
            base.entries.keys().chain(head.entries.keys()).collect();
        for name in names {
            // Shared trees count at every path, including directories with no leaves.
            entries_left = entries_left
                .checked_sub(1)
                .ok_or_else(|| invalid("tree diff entry limit exceeded"))?;
            let old = base.entries.get(name);
            let new = head.entries.get(name);
            if old == new {
                continue;
            }
            let path_len = prefix
                .len()
                .checked_add(usize::from(!prefix.is_empty()))
                .and_then(|len| len.checked_add(name.len()))
                .ok_or_else(|| invalid("tree diff path byte limit exceeded"))?;
            path_bytes_left = path_bytes_left
                .checked_sub(path_len)
                .ok_or_else(|| invalid("tree diff path byte limit exceeded"))?;
            let mut path = Vec::with_capacity(path_len);
            path.extend_from_slice(&prefix);
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            match (old, new) {
                (_, Some(TreeEntry::Leaf(entry))) => {
                    out.insert(RepoPath::new(path.clone())?, Some(entry.clone()));
                }
                (Some(TreeEntry::Leaf(_)), _) => {
                    out.insert(RepoPath::new(path.clone())?, None);
                }
                _ => {}
            }
            let old_tree = match old {
                Some(TreeEntry::SubTree(id)) => Some(*id),
                _ => None,
            };
            let new_tree = match new {
                Some(TreeEntry::SubTree(id)) => Some(*id),
                _ => None,
            };
            if old_tree.is_some() || new_tree.is_some() {
                stack.push((path, old_tree, new_tree));
            }
        }
    }
    Ok(out
        .into_iter()
        .map(|(path, entry)| match entry {
            Some(entry) => FileChange::Set { path, entry },
            None => FileChange::Delete { path },
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kasu_core::BlobId;
    use kasu_store::{FsBlobStore, SqliteStore};

    fn file() -> TreeEntry {
        TreeEntry::Leaf(Entry::File {
            blob: BlobId([1; 32]),
            executable: false,
        })
    }

    #[test]
    fn diff_counts_shared_subtrees_at_every_path_even_without_files() {
        let dir = tempfile::tempdir().unwrap();
        let meta = SqliteStore::memory().unwrap();
        let blobs = FsBlobStore::open(dir.path()).unwrap();
        let repo = Repository {
            meta: &meta,
            blobs: &blobs,
        };
        let empty = repo.put_tree(&Tree::default()).unwrap();
        for leaf in [
            Tree::default(),
            Tree {
                entries: BTreeMap::from([("f".into(), file())]),
            },
        ] {
            let mut root = repo.put_tree(&leaf).unwrap();
            let mut entries = leaf.entries.len();
            for _ in 0..4 {
                root = repo
                    .put_tree(&Tree {
                        entries: BTreeMap::from([
                            ("a".into(), TreeEntry::SubTree(root)),
                            ("b".into(), TreeEntry::SubTree(root)),
                        ]),
                    })
                    .unwrap();
                entries = 2 * (entries + 1);
            }
            let changes = diff_with_budget(&repo, empty, root, entries, 1024).unwrap();
            assert_eq!(changes.len(), 16 * leaf.entries.len());
            for (base, head) in [(empty, root), (root, empty)] {
                assert_eq!(
                    diff_with_budget(&repo, base, head, entries - 1, 1024)
                        .unwrap_err()
                        .to_string(),
                    invalid("tree diff entry limit exceeded").to_string()
                );
            }
            assert!(
                diff_with_budget(&repo, root, root, 0, 0)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn diff_bounds_cumulative_paths_and_preserves_unchanged_subtree_skips() {
        let dir = tempfile::tempdir().unwrap();
        let meta = SqliteStore::memory().unwrap();
        let blobs = FsBlobStore::open(dir.path()).unwrap();
        let repo = Repository {
            meta: &meta,
            blobs: &blobs,
        };
        let empty = repo.put_tree(&Tree::default()).unwrap();
        let child = repo
            .put_tree(&Tree {
                entries: BTreeMap::from([(BString::from(b"\xff".as_slice()), file())]),
            })
            .unwrap();
        let base = repo
            .put_tree(&Tree {
                entries: BTreeMap::from([
                    ("a".into(), TreeEntry::SubTree(child)),
                    ("bb".into(), TreeEntry::SubTree(child)),
                ]),
            })
            .unwrap();
        // Directory paths (1 + 2) and file paths (3 + 4) are charged separately.
        let changes = diff_with_budget(&repo, empty, base, 4, 10).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(apply(&repo, empty, &changes).unwrap(), base);
        assert_eq!(
            diff_with_budget(&repo, empty, base, 4, 9)
                .unwrap_err()
                .to_string(),
            invalid("tree diff path byte limit exceeded").to_string()
        );
        let mut head = repo.tree(base).unwrap();
        head.entries.insert("c".into(), file());
        let head = repo.put_tree(&head).unwrap();
        let changes = diff_with_budget(&repo, base, head, 3, 1).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path().as_bytes(), b"c");
        assert_eq!(apply(&repo, base, &changes).unwrap(), head);
    }
}
