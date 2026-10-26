use kasu_core as core;
use kasu_derived::{Blame, Ctx, Engine};
use kasu_git::{GitIndex, ProjectionId, register_git};
use kasu_ops::Operations;
use kasu_proto::v1 as p;
use kasu_store::{BlobStore, GitRefStore, MetaStore, Repository};
use std::sync::Arc;

#[derive(Clone)]
pub struct Backend {
    pub(crate) runtime: Arc<crate::admission::Runtime>,
    pub(crate) projection_accounts: Arc<Vec<crate::ProjectionAccount>>,
    pub(crate) repository_accounts: Arc<Vec<crate::RepositoryAccount>>,
    pub(crate) writer_account: Option<usize>,
    pub(crate) read_scope: Option<Arc<crate::read_scope::ReadScope>>,
    pub(crate) write_policy: crate::WritePolicy,
    pub(crate) receive_limits: kasu_git::ReceiveLimits,
    pub(crate) token: Option<String>,
    pub(crate) object_token: Option<String>,
    pub(crate) pack_cache: Arc<kasu_git::PackCache>,
    pub(crate) repository: Vec<u8>,
    meta: Arc<dyn MetaStore>,
    blobs: Arc<dyn BlobStore>,
    pub(crate) refs: Arc<dyn GitRefStore>,
    engine: Arc<Engine>,
}
impl Backend {
    pub fn git_cache_stats(&self) -> kasu_git::PackCacheStats {
        self.pack_cache.stats()
    }

    pub fn new(
        repository: Vec<u8>,
        meta: Arc<dyn MetaStore>,
        blobs: Arc<dyn BlobStore>,
        refs: Arc<dyn GitRefStore>,
        token: Option<String>,
    ) -> Self {
        let mut engine = Engine::standard();
        register_git(&mut engine);
        let stats = Arc::new(crate::admission::ServerStats::default());
        let runtime = crate::admission::Runtime::new(crate::ServerLimits::default(), stats.clone());
        let refs = Arc::new(crate::observed_refs::ObservedRefs { inner: refs, stats });
        Self {
            runtime,
            projection_accounts: Arc::new(Vec::new()),
            repository_accounts: Arc::new(Vec::new()),
            writer_account: None,
            read_scope: None,
            write_policy: crate::WritePolicy::default(),
            receive_limits: kasu_git::ReceiveLimits::default(),
            repository,
            meta,
            blobs,
            refs,
            engine: Arc::new(engine),
            token,
            object_token: None,
            pack_cache: Arc::new(kasu_git::PackCache::new(32 * 1024 * 1024)),
        }
    }
    pub fn with_limits(mut self, limits: crate::ServerLimits) -> Result<Self, tonic::Status> {
        limits.validate()?;
        self.runtime = crate::admission::Runtime::new(limits, self.runtime.stats.clone());
        Ok(self)
    }
    pub fn metrics(&self) -> String {
        let mut text = self.runtime.metrics();
        text.push_str(&self.engine.metrics());
        let stats = self.pack_cache.stats();
        text.push_str(&format!("kasu_server_git_cache_hits_total {}\nkasu_server_git_cache_misses_total {}\nkasu_server_git_cache_bytes {}\n",stats.hits,stats.misses,stats.bytes));
        text
    }
    pub fn with_object_token(mut self, token: String) -> Result<Self, tonic::Status> {
        kasu_object::bearer(&token)?;
        if self.token.as_deref() == Some(&token)
            || self.projection_accounts.iter().any(|a| a.token == token)
            || self.repository_accounts.iter().any(|a| a.token == token)
        {
            return Err(tonic::Status::invalid_argument(
                "object hop token must differ from user token",
            ));
        }
        self.object_token = Some(token);
        Ok(self)
    }
    pub(crate) fn ops(&self) -> Operations<'_> {
        Operations {
            engine: &self.engine,
            ctx: Ctx {
                repo: Repository {
                    meta: self.meta.as_ref(),
                    blobs: self.blobs.as_ref(),
                },
            },
        }
    }
    pub(crate) fn repo(&self, repo: &[u8]) -> Result<(), tonic::Status> {
        if repo != self.repository {
            Err(tonic::Status::permission_denied(
                "repository is not authorized",
            ))
        } else {
            Ok(())
        }
    }
    pub(crate) fn id(&self, repo: &[u8], raw: &[u8]) -> Result<core::ChangesetId, tonic::Status> {
        self.repo(repo)?;
        let id = core::ChangesetId(
            raw.try_into()
                .map_err(|_| tonic::Status::invalid_argument("changeset ID must be 32 bytes"))?,
        );
        self.ops()
            .ctx
            .repo
            .require_membership(repo, id)
            .map_err(status)?;
        Ok(id)
    }
    pub fn get_blob_metadata(
        &self,
        q: p::ObjectRequest,
    ) -> Result<p::BlobMetadataResponse, tonic::Status> {
        self.repo(&q.repo)?;
        if q.ids.is_empty() || q.ids.len() > 128 {
            return Err(tonic::Status::invalid_argument(
                "blob metadata batch must contain 1..128 IDs",
            ));
        }
        let repo = self.ops().ctx.repo;
        let mut blobs = Vec::with_capacity(q.ids.len());
        for raw in q.ids {
            let id = core::BlobId(
                raw.as_slice()
                    .try_into()
                    .map_err(|_| tonic::Status::invalid_argument("blob ID must be 32 bytes"))?,
            );
            repo.require_blob_membership(self.object_namespace(), id)
                .map_err(status)?;
            let metadata = repo.blob_metadata(id).map_err(status)?;
            blobs.push(p::BlobMetadata {
                id: raw,
                size: metadata.size,
                git_oid: metadata.git_blob_oid.0.to_vec(),
            });
        }
        Ok(p::BlobMetadataResponse { blobs })
    }
    pub fn get_changeset(&self, q: p::ChangesetRequest) -> Result<p::Changeset, tonic::Status> {
        let id = self.id(&q.repo, &q.id)?;
        let cs = self.ops().ctx.repo.changeset(id).map_err(status)?;
        self.read_changeset(changeset(id, cs))
    }
    pub fn get_root_tree(
        &self,
        q: p::ChangesetRequest,
    ) -> Result<p::RootTreeResponse, tonic::Status> {
        let id = self.id(&q.repo, &q.id)?;
        let root = self.ops().root_tree(id).map_err(status)?;
        let root = self.read_root(root)?;
        Ok(p::RootTreeResponse {
            id: root.0.to_vec(),
            read_filtered: self.read_scope.is_some(),
        })
    }
    pub fn resolve_ref(&self, q: p::RefRequest) -> Result<p::Ref, tonic::Status> {
        self.repo(&q.repo)?;
        let value = self.refs.resolve(&q.repo, &q.name).map_err(status)?;
        if let Some(target) = value.target {
            self.ops()
                .ctx
                .repo
                .require_membership(&q.repo, target)
                .map_err(status)?;
        }
        Ok(p::Ref {
            name: q.name,
            target: value.target.map(|id| id.0.to_vec()),
            version: value.version,
        })
    }
    pub fn list_refs(&self, q: p::ListRefsRequest) -> Result<p::ListRefsResponse, tonic::Status> {
        self.repo(&q.repo)?;
        let mut refs = Vec::new();
        for (name, state) in self.refs.list(&q.repo, &q.prefix).map_err(status)? {
            if let Some(id) = state.target {
                self.ops()
                    .ctx
                    .repo
                    .require_membership(&q.repo, id)
                    .map_err(status)?;
            }
            refs.push(p::Ref {
                name,
                target: state.target.map(|id| id.0.to_vec()),
                version: state.version,
            });
        }
        Ok(p::ListRefsResponse { refs })
    }
    pub fn log(&self, q: p::LogRequest) -> Result<p::LogResponse, tonic::Status> {
        let id = self.id(&q.repo, &q.start)?;
        let path = q
            .path
            .map(core::RepoPath::new)
            .transpose()
            .map_err(status)?;
        if let Some(path) = &path {
            self.require_read_path(path.as_bytes())?;
        }
        // Cursor targets are also checked for membership, including user-modified cursors.
        if !q.cursor.is_empty() {
            let raw =
                q.cursor
                    .get(
                        q.cursor.len().checked_sub(32).ok_or_else(|| {
                            tonic::Status::invalid_argument("invalid log cursor")
                        })?..,
                    )
                    .ok_or_else(|| tonic::Status::invalid_argument("invalid log cursor"))?;
            self.id(&q.repo, raw)?;
        }
        let page = self
            .ops()
            .log(id, path.as_ref(), q.limit as usize, &q.cursor)
            .map_err(status)?;
        let changesets = page
            .changesets
            .into_iter()
            .map(|id| {
                self.get_changeset(p::ChangesetRequest {
                    repo: q.repo.clone(),
                    id: id.0.to_vec(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(p::LogResponse {
            changesets,
            cursor: page.cursor,
        })
    }
    pub fn is_ancestor(&self, q: p::CompareRequest) -> Result<p::AncestorResponse, tonic::Status> {
        let a = self.id(&q.repo, &q.base)?;
        let b = self.id(&q.repo, &q.head)?;
        Ok(p::AncestorResponse {
            is_ancestor: self.ops().is_ancestor(a, b).map_err(status)?,
        })
    }
    pub fn merge_base(&self, q: p::CompareRequest) -> Result<p::MergeBaseResponse, tonic::Status> {
        let a = self.id(&q.repo, &q.base)?;
        let b = self.id(&q.repo, &q.head)?;
        Ok(p::MergeBaseResponse {
            ids: self
                .ops()
                .merge_bases(a, b)
                .map_err(status)?
                .into_iter()
                .map(|id| id.0.to_vec())
                .collect(),
        })
    }
    pub fn diff(&self, q: p::DiffRequest) -> Result<p::DiffResponse, tonic::Status> {
        let a = self.id(&q.repo, &q.base)?;
        let b = self.id(&q.repo, &q.head)?;
        let mut paths = q
            .paths
            .into_iter()
            .map(core::RepoPath::new)
            .collect::<core::Result<Vec<_>>>()
            .map_err(status)?;
        for path in &paths {
            self.require_read_path(path.as_bytes())?;
        }
        if paths.is_empty()
            && let Some(scope) = &self.read_scope
        {
            paths = scope.directories.clone();
        }
        let files = self
            .ops()
            .diff(a, b, &paths, q.content)
            .map_err(status)?
            .into_iter()
            .map(|f| {
                for value in f.before.iter().chain(f.after.iter()) {
                    if let Some(blob) = value.blob() {
                        self.grant_read_blob(blob)?;
                    }
                }
                Ok(p::FileDiff {
                    path: f.path.as_bytes().to_vec(),
                    before: f.before.map(entry),
                    after: f.after.map(entry),
                    binary: f.binary,
                    edits: f
                        .edits
                        .into_iter()
                        .map(|e| p::ContentEdit {
                            before_start: e.before_start,
                            after_start: e.after_start,
                            before: e.before,
                            after: e.after,
                        })
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>, tonic::Status>>()?;
        Ok(p::DiffResponse { files })
    }
    pub fn blame(&self, q: p::PathRequest) -> Result<p::BlameResponse, tonic::Status> {
        let id = self.id(&q.repo, &q.id)?;
        let path = core::RepoPath::new(q.path).map_err(status)?;
        self.require_read_path(path.as_bytes())?;
        let (value, _) = self.ops().read_file(id, &path).map_err(status)?;
        if let Some(blob) = value.blob() {
            self.grant_read_blob(blob)?;
        }
        let blame = if q.follow_copies {
            self.engine
                .derive::<kasu_derived::CopyBlame>(&self.ops().ctx, id, &path)
                .map_err(status)?
                .0
        } else if q.follow_moves {
            self.engine
                .derive::<kasu_derived::MovedBlame>(&self.ops().ctx, id, &path)
                .map_err(status)?
                .0
        } else {
            self.engine
                .derive::<Blame>(&self.ops().ctx, id, &path)
                .map_err(status)?
        };
        if blame
            .lines
            .iter()
            .any(|l| !self.read_allows(l.path.as_bytes()))
        {
            return Err(tonic::Status::permission_denied(
                "blame origin is outside readable directories",
            ));
        }
        Ok(p::BlameResponse {
            blob: blame.blob.map(|b| b.0.to_vec()).unwrap_or_default(),
            lines: blame
                .lines
                .into_iter()
                .map(|l| p::LineOrigin {
                    changeset: l.changeset.0.to_vec(),
                    path: l.path.as_bytes().to_vec(),
                    line: l.line,
                })
                .collect(),
        })
    }
    pub fn read_file(&self, q: p::PathRequest) -> Result<p::ReadFileResponse, tonic::Status> {
        let id = self.id(&q.repo, &q.id)?;
        let path = core::RepoPath::new(q.path).map_err(status)?;
        self.require_read_path(path.as_bytes())?;
        let (value, content) = self.ops().read_file(id, &path).map_err(status)?;
        if let Some(blob) = value.blob() {
            self.grant_read_blob(blob)?;
        }
        Ok(p::ReadFileResponse {
            entry: Some(entry(value)),
            content,
        })
    }
    pub fn list_dir(&self, q: p::PathRequest) -> Result<p::ListDirResponse, tonic::Status> {
        self.get_root_tree(p::ChangesetRequest {
            repo: q.repo.clone(),
            id: q.id.clone(),
        })?;
        let id = self.id(&q.repo, &q.id)?;
        let path = if q.path.is_empty() {
            None
        } else {
            Some(core::RepoPath::new(q.path).map_err(status)?)
        };
        if !self.read_enters(path.as_ref().map(|p| p.as_bytes()).unwrap_or_default()) {
            return Err(tonic::Status::permission_denied(
                "directory is outside readable scope",
            ));
        }
        let tree = if self.read_scope.is_some() {
            let root = self.read_root(self.ops().root_tree(id).map_err(status)?)?;
            let tree_id = match path.as_ref() {
                None => root,
                Some(path) => {
                    match kasu_derived::lookup(&self.ops().ctx.repo, root, path).map_err(status)? {
                        Some(core::TreeEntry::SubTree(id)) => id,
                        _ => return Err(tonic::Status::not_found("directory not found")),
                    }
                }
            };
            self.ops().ctx.repo.tree(tree_id).map_err(status)?
        } else {
            self.ops().list_dir(id, path.as_ref()).map_err(status)?
        };
        Ok(p::ListDirResponse {
            entries: tree
                .entries
                .into_iter()
                .map(|(name, value)| p::DirEntry {
                    name: name.to_vec(),
                    kind: Some(match value {
                        core::TreeEntry::Leaf(value) => p::dir_entry::Kind::Entry(entry(value)),
                        core::TreeEntry::SubTree(id) => p::dir_entry::Kind::Tree(id.0.to_vec()),
                    }),
                })
                .collect(),
        })
    }
    pub fn find_changeset_by_git_oid(
        &self,
        q: p::GitOidRequest,
    ) -> Result<p::FindChangesetResponse, tonic::Status> {
        self.repo(&q.repo)?;
        let oid = core::GitOid(
            q.oid
                .as_slice()
                .try_into()
                .map_err(|_| tonic::Status::invalid_argument("Git OID must be 20 bytes"))?,
        );
        let ops = self.ops();
        let index = GitIndex {
            repo: ops.ctx.repo,
            projection: ProjectionId::full(q.repo),
        };
        let id = index.find_changeset(oid).map_err(status)?;
        if let Some(id) = id {
            index
                .repo
                .require_membership(&self.repository, id)
                .map_err(status)?;
        }
        Ok(p::FindChangesetResponse {
            id: id.map(|id| id.0.to_vec()),
        })
    }
}
pub(crate) fn status(error: core::Error) -> tonic::Status {
    match error {
        core::Error::Denied(message) => tonic::Status::permission_denied(message),
        core::Error::Invalid(message) => tonic::Status::invalid_argument(message),
        core::Error::NotFound(message) => tonic::Status::not_found(message),
        core::Error::Precondition(message) => tonic::Status::failed_precondition(message),
        other => {
            tracing::error!(error=%other,"metadata operation failed");
            tonic::Status::internal("storage operation failed")
        }
    }
}
fn signature(s: core::Signature) -> p::Signature {
    p::Signature {
        name: s.name.to_vec(),
        email: s.email.to_vec(),
        timestamp: s.timestamp,
        tz: s.tz.to_vec(),
    }
}
pub(crate) fn entry(value: core::Entry) -> p::Entry {
    p::Entry {
        kind: Some(match value {
            core::Entry::File { blob, executable } => p::entry::Kind::File(p::File {
                blob: blob.0.to_vec(),
                executable,
            }),
            core::Entry::Symlink { blob } => p::entry::Kind::Symlink(blob.0.to_vec()),
            core::Entry::Gitlink { commit } => p::entry::Kind::Gitlink(commit.0.to_vec()),
        }),
    }
}
pub(crate) fn changeset(id: core::ChangesetId, cs: core::Changeset) -> p::Changeset {
    p::Changeset {
        id: id.0.to_vec(),
        parents: cs.parents.into_iter().map(|id| id.0.to_vec()).collect(),
        author: Some(signature(cs.author)),
        committer: Some(signature(cs.committer)),
        message: cs.message.to_vec(),
        changes: cs
            .changes
            .into_iter()
            .map(|c| match c {
                core::FileChange::Set { path, entry: value } => p::FileChange {
                    path: path.as_bytes().to_vec(),
                    entry: Some(entry(value)),
                },
                core::FileChange::Delete { path } => p::FileChange {
                    path: path.as_bytes().to_vec(),
                    entry: None,
                },
            })
            .collect(),
        raw_git_commit: cs.git_provenance.map(|p| p.raw_commit.0.to_vec()),
        read_filtered: false,
    }
}
