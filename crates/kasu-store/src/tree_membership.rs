use crate::Repository;
use kasu_core::{Error, ObjectId, Result, TreeEntry, TreeId};

impl Repository<'_> {
    fn tree_membership_key(repo: &[u8], id: TreeId) -> Vec<u8> {
        let mut encoded = kasu_core::Encoder::default();
        encoded.bytes(repo);
        let prefix = ObjectId::hash(&encoded.0);
        format!("repo/tree/v1/{prefix}/{id}").into_bytes()
    }
    pub fn has_tree_membership(&self, repo: &[u8], id: TreeId) -> Result<bool> {
        Ok(self
            .meta
            .get(&Self::tree_membership_key(repo, id))?
            .is_some())
    }
    pub fn require_tree_membership(&self, repo: &[u8], id: TreeId) -> Result<()> {
        if !self.has_tree_membership(repo, id)? {
            return Err(Error::NotFound(format!("tree {id} in repository")));
        }
        Ok(())
    }
    /// The caller must first authorize the changeset that owns this root.
    /// Publish parents only after validating children. Previously associated
    /// immutable subtrees are skipped, including after a process restart.
    pub fn associate_tree_graph(&self, repo: &[u8], root: TreeId) -> Result<()> {
        let mut stack = vec![(root, false)];
        let mut seen = std::collections::BTreeSet::new();
        while let Some((id, finished)) = stack.pop() {
            if finished {
                self.meta
                    .put_if_absent(&Self::tree_membership_key(repo, id), &[])?;
            } else if !self.has_tree_membership(repo, id)? && seen.insert(id) {
                let tree = self.tree(id)?;
                stack.push((id, true));
                for entry in tree.entries.values() {
                    match entry {
                        TreeEntry::SubTree(child) => stack.push((*child, false)),
                        TreeEntry::Leaf(entry) => {
                            if let Some(blob) = entry.blob() {
                                self.associate_blob(repo, blob)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
