use kasu_core::{BString, Entry, FileChange, RepoPath, Result, Tree, TreeEntry, TreeId, invalid};
use kasu_store::Repository;
use std::collections::BTreeMap;

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
        let names: std::collections::BTreeSet<_> =
            base.entries.keys().chain(head.entries.keys()).collect();
        for name in names {
            let old = base.entries.get(name);
            let new = head.entries.get(name);
            if old == new {
                continue;
            }
            let mut path = prefix.clone();
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
