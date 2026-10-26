use crate::{Backend, auth::Access, backend::status};
use kasu_core::{BlobId, Encoder, ObjectId, RepoPath, Tree, TreeEntry, TreeId};
use kasu_proto::v1 as p;
use std::{collections::BTreeMap, sync::Arc};
use tonic::Status;

pub(crate) struct ReadScope {
    pub directories: Vec<RepoPath>,
    pub namespace: Vec<u8>,
}
impl ReadScope {
    pub fn new(repository: &[u8], directories: &[RepoPath]) -> Self {
        let mut encoded = Encoder::default();
        encoded.bytes(repository);
        for path in directories {
            encoded.bytes(path.as_bytes());
        }
        let mut namespace = b"kasu.native.read.v1\0".to_vec();
        namespace.extend(ObjectId::hash(&encoded.0).0);
        Self {
            directories: directories.to_vec(),
            namespace,
        }
    }
}
pub(crate) fn below(path: &[u8], prefix: &[u8]) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|s| s.starts_with(b"/"))
}
impl Backend {
    pub(crate) fn reader(&self, authorization: Option<&str>) -> Result<Arc<Self>, Status> {
        let mut scoped = self.clone();
        scoped.read_scope = match self.authenticate(authorization)? {
            Access::Repository => None,
            Access::Account(a) => a
                .read_directories
                .as_ref()
                .map(|dirs| Arc::new(ReadScope::new(&self.repository, dirs))),
            Access::Projection(_) => {
                return Err(Status::permission_denied(
                    "projection credentials require a Git projection endpoint",
                ));
            }
        };
        Ok(Arc::new(scoped))
    }
    pub(crate) fn object_reader(&self, authorization: Option<&str>) -> Result<Arc<Self>, Status> {
        if let Some(token) = &self.object_token {
            let expected = format!("Bearer {token}");
            if authorization
                .is_some_and(|v| kasu_object::token_eq(v.as_bytes(), expected.as_bytes()))
            {
                return Ok(Arc::new(self.clone()));
            }
            let reader = self.reader(authorization)?;
            if reader.read_scope.is_none() {
                return Err(Status::unauthenticated("object hop token required"));
            }
            return Ok(reader);
        }
        self.reader(authorization)
    }
    pub(crate) fn read_allows(&self, path: &[u8]) -> bool {
        self.read_scope
            .as_ref()
            .is_none_or(|s| s.directories.iter().any(|p| below(path, p.as_bytes())))
    }
    pub(crate) fn read_enters(&self, path: &[u8]) -> bool {
        path.is_empty()
            || self.read_allows(path)
            || self
                .read_scope
                .as_ref()
                .is_some_and(|s| s.directories.iter().any(|p| below(p.as_bytes(), path)))
    }
    pub(crate) fn require_read_path(&self, path: &[u8]) -> Result<(), Status> {
        if self.read_allows(path) {
            Ok(())
        } else {
            Err(Status::permission_denied(
                "path is outside readable directories",
            ))
        }
    }
    pub(crate) fn object_namespace(&self) -> &[u8] {
        self.read_scope
            .as_ref()
            .map(|s| s.namespace.as_slice())
            .unwrap_or(&self.repository)
    }
    pub(crate) fn grant_read_blob(&self, id: BlobId) -> Result<(), Status> {
        self.ops()
            .ctx
            .repo
            .associate_blob(self.object_namespace(), id)
            .map_err(status)
    }
    pub(crate) fn read_changeset(&self, mut cs: p::Changeset) -> Result<p::Changeset, Status> {
        if self.read_scope.is_some() {
            cs.read_filtered = true;
            cs.raw_git_commit = None;
            cs.changes.retain(|c| self.read_allows(&c.path));
            for change in &cs.changes {
                let blob = change.entry.as_ref().and_then(|e| match e.kind.as_ref() {
                    Some(p::entry::Kind::File(f)) => Some(&f.blob),
                    Some(p::entry::Kind::Symlink(id)) => Some(id),
                    _ => None,
                });
                if let Some(id) = blob {
                    self.grant_read_blob(BlobId(
                        id.as_slice()
                            .try_into()
                            .map_err(|_| Status::internal("invalid stored blob"))?,
                    ))?;
                }
            }
        }
        Ok(cs)
    }
    // Only boundary directories are rewritten. Fully readable subtrees keep their IDs.
    // The grant index is shared by equivalent scopes and published after validation.
    pub(crate) fn read_root(&self, root: TreeId) -> Result<TreeId, Status> {
        let repo = self.ops().ctx.repo;
        let Some(scope) = &self.read_scope else {
            repo.associate_tree_graph(&self.repository, root)
                .map_err(status)?;
            return Ok(root);
        };
        let mut key = b"native/read-root/v1/".to_vec();
        key.extend(ObjectId::hash(&scope.namespace).0);
        key.extend(root.0);
        if let Some(raw) = repo.meta.get(&key).map_err(status)? {
            return Ok(TreeId(
                raw.try_into()
                    .map_err(|_| Status::internal("invalid read root cache"))?,
            ));
        }
        let empty = repo.put_tree(&Tree::default()).map_err(status)?;
        let mut results = BTreeMap::<Vec<u8>, TreeId>::new();
        let mut stack = vec![(vec![], root, false)];
        while let Some((path, id, finished)) = stack.pop() {
            if self.read_allows(&path) {
                results.insert(path, id);
                continue;
            }
            let mut tree = repo.tree(id).map_err(status)?;
            if !finished {
                stack.push((path.clone(), id, true));
                for (name, entry) in &tree.entries {
                    let child = child_path(&path, name);
                    if let TreeEntry::SubTree(id) = entry
                        && self.read_enters(&child)
                        && !self.read_allows(&child)
                    {
                        stack.push((child, *id, false));
                    }
                }
            } else {
                tree.entries.retain(|name, entry| {
                    let child = child_path(&path, name);
                    if self.read_allows(&child) {
                        return true;
                    }
                    if matches!(entry, TreeEntry::SubTree(_))
                        && let Some(id) = results.get(&child)
                        && *id != empty
                    {
                        *entry = TreeEntry::SubTree(*id);
                        return true;
                    }
                    false
                });
                results.insert(path, repo.put_tree(&tree).map_err(status)?);
            }
        }
        let filtered = results
            .remove(b"".as_slice())
            .ok_or_else(|| Status::internal("missing filtered root"))?;
        repo.associate_tree_graph(&scope.namespace, filtered)
            .map_err(status)?;
        repo.meta.put_if_absent(&key, &filtered.0).map_err(status)?;
        Ok(filtered)
    }
}
fn child_path(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        name.to_vec()
    } else {
        [parent, b"/", name].concat()
    }
}
