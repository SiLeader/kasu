use crate::{
    Backend,
    backend::{changeset, entry, status},
};
use kasu_core as c;
use kasu_ops::{CommitMetadata, RewriteResult};
use kasu_proto::v1 as p;
use kasu_store::{CasResult, RefState, RefUpdate};
use tonic::Status;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WritePolicy {
    #[serde(deserialize_with = "byte_rules")]
    pub allowed_prefixes: Vec<Vec<u8>>,
    #[serde(deserialize_with = "byte_rules")]
    pub force_prefixes: Vec<Vec<u8>>,
    #[serde(deserialize_with = "byte_rules")]
    pub allowed_ref_patterns: Vec<Vec<u8>>,
    #[serde(deserialize_with = "byte_rules")]
    pub force_ref_patterns: Vec<Vec<u8>>,
    #[serde(deserialize_with = "byte_rules")]
    pub allowed_path_prefixes: Vec<Vec<u8>>,
    pub allow_create: bool,
    pub allow_delete: bool,
}
fn byte_rules<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Rule {
        Text(String),
        Bytes(Vec<u8>),
    }
    let rules: Vec<Rule> = serde::Deserialize::deserialize(deserializer)?;
    Ok(rules
        .into_iter()
        .map(|rule| match rule {
            Rule::Text(s) => s.into_bytes(),
            Rule::Bytes(b) => b,
        })
        .collect())
}
impl Default for WritePolicy {
    fn default() -> Self {
        Self {
            allowed_prefixes: vec![b"refs/heads/".to_vec(), b"refs/tags/".to_vec()],
            force_prefixes: vec![],
            allowed_ref_patterns: vec![],
            force_ref_patterns: vec![],
            allowed_path_prefixes: vec![vec![]],
            allow_create: true,
            allow_delete: false,
        }
    }
}
impl WritePolicy {
    pub fn allows_ref(&self, name: &[u8]) -> bool {
        if self.allowed_ref_patterns.is_empty() {
            self.allowed_prefixes.iter().any(|p| name.starts_with(p))
        } else {
            self.allowed_ref_patterns.iter().any(|p| glob(p, name))
        }
    }
    pub fn allows_force(&self, name: &[u8]) -> bool {
        if self.force_ref_patterns.is_empty() {
            self.force_prefixes.iter().any(|p| name.starts_with(p))
        } else {
            self.force_ref_patterns.iter().any(|p| glob(p, name))
        }
    }
    pub fn unrestricted_paths(&self) -> bool {
        self.allowed_path_prefixes.iter().any(|p| p.is_empty())
    }
    pub fn allows_path(&self, path: &[u8]) -> bool {
        self.allowed_path_prefixes.iter().any(|p| {
            let p = p.strip_suffix(b"/").unwrap_or(p);
            p.is_empty() || path == p || path.strip_prefix(p).is_some_and(|s| s.starts_with(b"/"))
        })
    }
}
pub(crate) fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t, mut star, mut retry) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if let Some(s) = star {
            retry += 1;
            t = retry;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
fn raw_id<const N: usize>(bytes: &[u8]) -> Result<[u8; N], Status> {
    bytes
        .try_into()
        .map_err(|_| Status::invalid_argument(format!("ID must be {N} bytes")))
}
fn signature(s: Option<p::Signature>) -> Result<c::Signature, Status> {
    let s = s.ok_or_else(|| Status::invalid_argument("signature is required"))?;
    Ok(c::Signature {
        name: s.name.into(),
        email: s.email.into(),
        timestamp: s.timestamp,
        tz: s.tz.into(),
    })
}
fn metadata(m: Option<p::CommitMetadata>) -> Result<CommitMetadata, Status> {
    let m = m.ok_or_else(|| Status::invalid_argument("metadata is required"))?;
    Ok(CommitMetadata {
        author: signature(m.author)?,
        committer: signature(m.committer)?,
        message: m.message.into(),
    })
}
fn decode_entry(value: p::Entry) -> Result<c::Entry, Status> {
    Ok(
        match value
            .kind
            .ok_or_else(|| Status::invalid_argument("entry kind is required"))?
        {
            p::entry::Kind::File(f) => c::Entry::File {
                blob: c::BlobId(raw_id(&f.blob)?),
                executable: f.executable,
            },
            p::entry::Kind::Symlink(id) => c::Entry::Symlink {
                blob: c::BlobId(raw_id(&id)?),
            },
            p::entry::Kind::Gitlink(id) => c::Entry::Gitlink {
                commit: c::GitOid(raw_id(&id)?),
            },
        },
    )
}
fn merge_entry(e: Option<c::TreeEntry>) -> Option<p::MergeEntry> {
    e.map(|e| p::MergeEntry {
        kind: Some(match e {
            c::TreeEntry::Leaf(e) => p::merge_entry::Kind::Leaf(entry(e)),
            c::TreeEntry::SubTree(t) => p::merge_entry::Kind::Tree(t.0.to_vec()),
        }),
    })
}
fn reference(name: Vec<u8>, state: RefState) -> p::Ref {
    p::Ref {
        name,
        target: state.target.map(|id| id.0.to_vec()),
        version: state.version,
    }
}
impl Backend {
    pub(crate) fn check_tree_acl(&self, old: c::TreeId, new: c::TreeId) -> Result<(), Status> {
        if self.unrestricted_paths() {
            return Ok(());
        }
        if kasu_derived::diff(&self.ops().ctx.repo, old, new)
            .map_err(status)?
            .iter()
            .any(|c| !self.allows_path(c.path().as_bytes()))
        {
            return Err(Status::permission_denied(
                "final tree changes a path that is not writable",
            ));
        }
        Ok(())
    }
    pub(crate) fn check_changeset_acl(
        &self,
        old: Option<c::ChangesetId>,
        new: Option<c::ChangesetId>,
    ) -> Result<(), Status> {
        if self.unrestricted_paths() {
            return Ok(());
        }
        let ops = self.ops();
        let empty = ops.ctx.repo.put_tree(&c::Tree::default()).map_err(status)?;
        let old = old
            .map(|id| ops.root_tree(id))
            .transpose()
            .map_err(status)?
            .unwrap_or(empty);
        let new = new
            .map(|id| ops.root_tree(id))
            .transpose()
            .map_err(status)?
            .unwrap_or(empty);
        self.check_tree_acl(old, new)
    }
    pub fn with_write_policy(mut self, policy: WritePolicy) -> Self {
        self.write_policy = policy;
        self
    }

    pub(crate) fn created(&self, id: c::ChangesetId) -> Result<p::Changeset, Status> {
        let ops = self.ops();
        let cs = ops.ctx.repo.changeset(id).map_err(status)?;
        for change in &cs.changes {
            if let c::FileChange::Set { entry, .. } = change
                && let Some(blob) = entry.blob()
            {
                ops.ctx
                    .repo
                    .associate_blob(self.key(), blob)
                    .map_err(status)?;
            }
        }
        ops.ctx
            .repo
            .associate_changeset(self.key(), id)
            .map_err(status)?;
        self.read_changeset(changeset(id, cs))
    }
    pub fn create_changeset(&self, q: p::CreateChangesetRequest) -> Result<p::Changeset, Status> {
        self.repo(&q.repo)?;
        if q.parents.len() > 64 || q.changes.len() > 100_000 {
            return Err(Status::resource_exhausted("too many parents or changes"));
        }
        let parents = q
            .parents
            .iter()
            .map(|raw| self.id(&q.repo, raw))
            .collect::<Result<Vec<_>, _>>()?;
        if parents
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != parents.len()
        {
            return Err(Status::invalid_argument("duplicate parents"));
        }
        let metadata = metadata(q.metadata)?;
        let mut changes = Vec::new();
        for change in q.changes {
            let path = c::RepoPath::new(change.path).map_err(status)?;
            self.require_read_path(path.as_bytes())?;
            changes.push(match change.entry {
                None => c::FileChange::Delete { path },
                Some(value) => {
                    let entry = decode_entry(value)?;
                    if let Some(blob) = entry.blob() {
                        self.ops()
                            .ctx
                            .repo
                            .require_blob_membership(self.object_namespace(), blob)
                            .map_err(status)?;
                    }
                    c::FileChange::Set { path, entry }
                }
            });
        }
        let cs = c::Changeset {
            parents,
            author: metadata.author,
            committer: metadata.committer,
            message: metadata.message,
            changes,
            git_provenance: None,
        };
        self.created(self.ops().create_changeset(&cs).map_err(status)?)
    }
    pub(crate) fn rewritten(&self, result: RewriteResult) -> Result<p::RewriteResponse, Status> {
        match result {
            RewriteResult::Created { id, bases } => {
                self.created(id)?;
                Ok(p::RewriteResponse {
                    id: Some(id.0.to_vec()),
                    bases: bases.into_iter().map(|id| id.0.to_vec()).collect(),
                    stage: p::ConflictStage::Result as i32,
                    conflicts: vec![],
                })
            }
            RewriteResult::Conflicted {
                stage,
                bases,
                conflicts,
            } => {
                if conflicts
                    .iter()
                    .any(|c| !self.read_allows(c.path.as_bytes()))
                {
                    return Err(Status::permission_denied(
                        "rewrite conflict is outside readable directories",
                    ));
                }
                // Conflicts can expose trees or blobs produced while reducing
                // virtual merge bases, without a published changeset root.
                for conflict in &conflicts {
                    for value in [&conflict.base, &conflict.ours, &conflict.theirs]
                        .into_iter()
                        .flatten()
                    {
                        match value {
                            c::TreeEntry::SubTree(id) => self
                                .ops()
                                .ctx
                                .repo
                                .associate_tree_graph(self.key(), *id)
                                .map_err(status)?,
                            c::TreeEntry::Leaf(entry) => {
                                if let Some(blob) = entry.blob() {
                                    self.ops()
                                        .ctx
                                        .repo
                                        .associate_blob(self.key(), blob)
                                        .map_err(status)?;
                                }
                            }
                        }
                        if self.read_scope.is_some() {
                            match value {
                                c::TreeEntry::SubTree(id) => self
                                    .ops()
                                    .ctx
                                    .repo
                                    .associate_tree_graph(self.object_namespace(), *id)
                                    .map_err(status)?,
                                c::TreeEntry::Leaf(entry) => {
                                    if let Some(blob) = entry.blob() {
                                        self.grant_read_blob(blob)?;
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(p::RewriteResponse {
                    id: None,
                    bases: bases.into_iter().map(|id| id.0.to_vec()).collect(),
                    stage: match stage {
                        kasu_ops::ConflictStage::Result => p::ConflictStage::Result,
                        kasu_ops::ConflictStage::MergeBases => p::ConflictStage::MergeBases,
                    } as i32,
                    conflicts: conflicts
                        .into_iter()
                        .map(|conflict| p::TreeConflict {
                            path: conflict.path.as_bytes().to_vec(),
                            kind: match conflict.kind {
                                kasu_ops::ConflictKind::Content => p::ConflictKind::Content,
                                kasu_ops::ConflictKind::Binary => p::ConflictKind::Binary,
                                kasu_ops::ConflictKind::AddAdd => p::ConflictKind::AddAdd,
                                kasu_ops::ConflictKind::DeleteModify => {
                                    p::ConflictKind::DeleteModify
                                }
                                kasu_ops::ConflictKind::FileDirectory => {
                                    p::ConflictKind::FileDirectory
                                }
                                kasu_ops::ConflictKind::TypeChange => p::ConflictKind::TypeChange,
                                kasu_ops::ConflictKind::Symlink => p::ConflictKind::Symlink,
                                kasu_ops::ConflictKind::Gitlink => p::ConflictKind::Gitlink,
                            } as i32,
                            base: merge_entry(conflict.base),
                            ours: merge_entry(conflict.ours),
                            theirs: merge_entry(conflict.theirs),
                            lines: conflict
                                .lines
                                .into_iter()
                                .map(|l| p::LineConflict {
                                    base_start: l.base.start as u64,
                                    base_end: l.base.end as u64,
                                    ours: l.ours,
                                    theirs: l.theirs,
                                })
                                .collect(),
                        })
                        .collect(),
                })
            }
        }
    }
    pub fn merge(&self, q: p::MergeRequest) -> Result<p::RewriteResponse, Status> {
        let a = self.id(&q.repo, &q.a)?;
        let b = self.id(&q.repo, &q.b)?;
        self.rewritten(
            self.ops()
                .merge(a, b, metadata(q.metadata)?)
                .map_err(status)?,
        )
    }
    pub fn rebase(&self, q: p::RebaseRequest) -> Result<p::RewriteResponse, Status> {
        let id = self.id(&q.repo, &q.id)?;
        let onto = self.id(&q.repo, &q.onto)?;
        self.rewritten(
            self.ops()
                .rebase(id, onto, signature(q.committer)?)
                .map_err(status)?,
        )
    }
    pub fn update_ref(&self, q: p::UpdateRefRequest) -> Result<p::UpdateRefResponse, Status> {
        self.repo(&q.repo)?;
        kasu_git::validate_branch_name(&q.name).map_err(status)?;
        let policy = self;
        if !policy.allows_ref(&q.name) {
            return Err(Status::permission_denied("ref is not writable"));
        }
        let expected = q
            .expected
            .ok_or_else(|| Status::invalid_argument("expected ref state is required"))?;
        if expected.name != q.name {
            return Err(Status::invalid_argument("expected ref name differs"));
        }
        let expected = RefState {
            target: expected
                .target
                .as_ref()
                .map(|id| self.id(&q.repo, id))
                .transpose()?,
            version: expected.version,
        };
        let new = q
            .target
            .as_ref()
            .map(|id| self.id(&q.repo, id))
            .transpose()?;
        if new.is_none() && !policy.allows_delete() {
            return Err(Status::permission_denied("ref deletion is disabled"));
        }
        if expected.target.is_none() && new.is_some() && !policy.allows_create() {
            return Err(Status::permission_denied("ref creation is disabled"));
        }
        if q.force && !policy.allows_force(&q.name) {
            return Err(Status::permission_denied(
                "force update is disabled for this ref",
            ));
        }
        let actual = self.refs.resolve(self.key(), &q.name).map_err(status)?;
        if actual != expected {
            return Ok(p::UpdateRefResponse {
                applied: false,
                actual: Some(reference(q.name, actual)),
            });
        }
        if let (Some(old), Some(new)) = (expected.target, new)
            && !q.force
            && !self.ops().is_ancestor(old, new).map_err(status)?
        {
            return Err(Status::permission_denied("update must be a fast-forward"));
        }
        if let Some(new) = new {
            // Branches have one mutable source of truth: native CAS. Preparing
            // the immutable Git projection also rejects unexportable identities.
            let ops = self.ops();
            ops.engine
                .derive::<kasu_git::GitCommit>(&ops.ctx, new, &self.full_projection())
                .map_err(status)?;
        }
        self.check_changeset_acl(expected.target, new)?;
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Status::internal("clock before epoch"))?
            .as_secs() as i64;
        let outcome = self
            .refs
            .compare_and_swap(
                self.key(),
                &[RefUpdate {
                    name: q.name.clone(),
                    expected,
                    new,
                }],
                self.write_actor(b"api"),
                time,
            )
            .map_err(status)?;
        Ok(match outcome {
            CasResult::Applied(states) => p::UpdateRefResponse {
                applied: true,
                actual: Some(reference(q.name, states[0])),
            },
            CasResult::Mismatch { actual, .. } => p::UpdateRefResponse {
                applied: false,
                actual: Some(reference(q.name, actual)),
            },
        })
    }
}

struct PendingBlob {
    id: c::BlobId,
    offset: u64,
    bytes: Vec<u8>,
    existing: bool,
}
#[derive(Default)]
pub(crate) struct UploadBuffer {
    pending: Option<PendingBlob>,
    receipts: Vec<p::BlobReceipt>,
    total: u64,
    chunks: u64,
}
impl UploadBuffer {
    pub(crate) fn push(&mut self, backend: &Backend, q: p::BlobUpload) -> Result<(), Status> {
        backend.repo(&q.repo)?;
        let id = c::BlobId(raw_id(&q.id)?);
        self.chunks += 1;
        self.total = self.total.saturating_add(q.data.len() as u64);
        if self.chunks > 65_536 || self.total > 512 * 1024 * 1024 || self.receipts.len() >= 4096 {
            return Err(Status::resource_exhausted("upload stream limit exceeded"));
        }
        let repo = backend.ops().ctx.repo;
        if self.pending.is_none() {
            self.pending = Some(PendingBlob {
                id,
                offset: 0,
                bytes: vec![],
                existing: repo
                    .has_blob_membership(backend.object_namespace(), id)
                    .map_err(status)?,
            });
        }
        let pending = self.pending.as_mut().unwrap();
        if pending.id != id || pending.offset != q.offset {
            return Err(Status::invalid_argument("noncontiguous blob ID or offset"));
        }
        pending.offset = pending.offset.saturating_add(q.data.len() as u64);
        if pending.offset > kasu_store::MAX_OBJECT_SIZE {
            return Err(Status::resource_exhausted("blob too large"));
        }
        if !pending.existing {
            pending.bytes.extend(q.data);
        }
        if q.last {
            let pending = self.pending.take().unwrap();
            let size = if pending.existing {
                repo.blob_metadata(id).map_err(status)?.size
            } else {
                if c::BlobId(c::ObjectId::hash(&pending.bytes).0) != id {
                    return Err(Status::invalid_argument("blob ID does not match content"));
                }
                repo.put_blob(&pending.bytes).map_err(status)?;
                repo.associate_blob(backend.key(), id).map_err(status)?;
                backend.grant_read_blob(id)?;
                pending.bytes.len() as u64
            };
            self.receipts.push(p::BlobReceipt {
                id: id.0.to_vec(),
                size,
                existing: pending.existing,
            });
        }
        Ok(())
    }
    pub(crate) fn finish(self) -> Result<p::UploadBlobsResponse, Status> {
        if self.pending.is_some() {
            return Err(Status::invalid_argument("unfinished blob"));
        }
        Ok(p::UploadBlobsResponse {
            blobs: self.receipts,
        })
    }
}
