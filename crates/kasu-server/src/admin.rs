use crate::{
    Backend, ConfigSource, RepositoryConfig,
    auth::{Credential, not_found},
    backend::status,
    registry::now_seconds,
};
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use kasu_core::{ChangesetId, Encoder, ObjectId, RepoKey, RepoName, RepositoryState, TreeEntry};
use kasu_git::{GitCommit, GitIndex, ProjectionId};
use kasu_proto::v1::{
    self as p,
    admin_server::{Admin, AdminServer},
};
use kasu_store::{RefState, RefUpdate, Repository, RepositoryRecord, RepositoryWrite};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tonic::{Request, Response, Status};

/// Server-wide repository lifecycle settings.
#[derive(Clone, Debug)]
pub struct Lifecycle {
    /// How long a renamed repository keeps its old name; None disables aliases.
    pub alias_ttl: Option<Duration>,
    /// How long the ref logs of a deleted repository are kept for audit.
    pub ref_log_retention: Duration,
    /// Who owns configuration when a repositories file is in use.
    pub config_source: Option<ConfigSource>,
}
impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            alias_ttl: Some(Duration::from_secs(30 * 24 * 3600)),
            ref_log_retention: Duration::from_secs(90 * 24 * 3600),
            config_source: None,
        }
    }
}
const MAX_USAGE_OBJECTS: usize = 1_000_000;
const PURGE_BATCH: usize = 10_000;

fn state_code(state: RepositoryState) -> p::RepositoryState {
    match state {
        RepositoryState::Active => p::RepositoryState::Active,
        RepositoryState::ReadOnly => p::RepositoryState::ReadOnly,
        RepositoryState::Archived => p::RepositoryState::Archived,
        RepositoryState::Deleting => p::RepositoryState::Deleting,
    }
}
fn name(value: &str) -> Result<RepoName, Status> {
    RepoName::new(value).map_err(|_| Status::invalid_argument("invalid repository name"))
}
fn branch(value: &[u8]) -> Result<Vec<u8>, Status> {
    let value = if value.is_empty() {
        kasu_store::DEFAULT_BRANCH
    } else {
        value
    };
    kasu_git::validate_branch_name(&[b"refs/heads/".as_slice(), value].concat())
        .map_err(|_| Status::invalid_argument("invalid default branch"))?;
    Ok(value.to_vec())
}
fn fingerprint(parts: &[&[u8]]) -> Vec<u8> {
    let mut encoded = Encoder::default();
    for part in parts {
        encoded.bytes(part);
    }
    ObjectId::hash(&encoded.0).0.to_vec()
}
impl Backend {
    pub fn with_lifecycle(mut self, lifecycle: crate::Lifecycle) -> Self {
        self.lifecycle = Arc::new(lifecycle);
        self
    }
    fn admin_credential(&self, authorization: Option<&str>) -> Result<Credential, Status> {
        self.credential(authorization, false)
    }
    fn require_operator(&self, credential: Credential) -> Result<&'static [u8], Status> {
        match credential {
            Credential::Anonymous => Ok(b"anonymous"),
            Credential::Operator => Ok(b"operator"),
            _ => Err(Status::permission_denied(
                "repository administration requires operator credentials",
            )),
        }
    }
    fn visible(&self, credential: Credential, record: &RepositoryRecord) -> bool {
        match credential {
            Credential::Anonymous | Credential::Operator => true,
            Credential::Account(account) => self.repository_accounts[account]
                .grant_for(record, self.legacy.as_ref())
                .is_some(),
            Credential::Projection(account) => self.projection_accounts[account]
                .scopes
                .iter()
                .any(|s| s.repository.matches(record, self.legacy.as_ref())),
            Credential::Hop => false,
        }
    }
    fn describe(&self, credential: Credential, record: &RepositoryRecord) -> p::Repository {
        let operator = matches!(credential, Credential::Anonymous | Credential::Operator);
        p::Repository {
            name: record.name.to_string(),
            key: record.key.as_bytes().to_vec(),
            state: state_code(record.state) as i32,
            default_branch: record.default_branch.clone(),
            git_commit_rule: record.git_commit_rule,
            config_version: record.config_version,
            // Webhook URLs and secret paths are for operators only.
            config_json: if operator {
                String::from_utf8_lossy(&record.config).into_owned()
            } else {
                String::new()
            },
            created_at: record.created_at,
            created_by: record.created_by.clone(),
            former_names: record.former_names.iter().map(|n| n.to_string()).collect(),
            replayed: false,
        }
    }
    /// The current record of a live name or alias, bypassing the cache.
    fn lookup(&self, value: &str) -> Result<RepositoryRecord, Status> {
        let Ok(name) = RepoName::new(value) else {
            return Err(not_found());
        };
        let store = self.registry.store();
        let entry = store
            .resolve_repository_name(&name, now_seconds())
            .map_err(status)?
            .ok_or_else(not_found)?;
        store
            .repository(&entry.key)
            .map_err(status)?
            .filter(|r| r.state != RepositoryState::Deleting)
            .ok_or_else(not_found)
    }
    fn lookup_visible(
        &self,
        credential: Credential,
        value: &str,
    ) -> Result<RepositoryRecord, Status> {
        let record = self.lookup(value)?;
        if !self.visible(credential, &record) {
            return Err(not_found());
        }
        Ok(record)
    }
    fn written(&self, write: RepositoryWrite) -> Result<RepositoryRecord, Status> {
        match write {
            RepositoryWrite::Applied(record) => {
                self.registry.invalidate(&record.key);
                Ok(record)
            }
            RepositoryWrite::NameTaken => Err(Status::already_exists("repository name is taken")),
            RepositoryWrite::Mismatch(_) => Err(Status::aborted(
                "repository changed concurrently; retry with the current version",
            )),
        }
    }
    pub fn create_repository(
        &self,
        authorization: Option<&str>,
        q: p::CreateRepositoryRequest,
    ) -> Result<p::Repository, Status> {
        let credential = self.admin_credential(authorization)?;
        let actor = self.require_operator(credential)?;
        let name = name(&q.name)?;
        let default_branch = branch(&q.default_branch)?;
        let config = RepositoryConfig::parse_request(q.config_json.as_bytes())?.encode();
        if q.idempotency_key.len() > 256 {
            return Err(Status::invalid_argument(
                "idempotency key exceeds 256 bytes",
            ));
        }
        let creation_key = if q.idempotency_key.is_empty() {
            Vec::new()
        } else {
            fingerprint(&[
                b"kasu.admin.create.v1",
                &q.idempotency_key,
                name.normalized().as_bytes(),
                &default_branch,
                &config,
            ])
        };
        let now = now_seconds();
        let mut record =
            RepositoryRecord::new(RepoKey::generate().map_err(status)?, name, actor, now);
        record.default_branch = default_branch;
        record.config = config;
        record.config_version = 1;
        record.creation_key = creation_key.clone();
        let store = self.registry.store();
        match store.create_repository(&record).map_err(status)? {
            RepositoryWrite::NameTaken if !creation_key.is_empty() => {
                let existing = self
                    .lookup(&q.name)
                    .map_err(|_| Status::already_exists("repository name is taken"))?;
                if existing.creation_key != creation_key {
                    return Err(Status::already_exists("repository name is taken"));
                }
                let mut response = self.describe(credential, &existing);
                response.replayed = true;
                Ok(response)
            }
            write => Ok(self.describe(credential, &self.written(write)?)),
        }
    }
    pub fn get_repository(
        &self,
        authorization: Option<&str>,
        q: p::GetRepositoryRequest,
    ) -> Result<p::Repository, Status> {
        let credential = self.admin_credential(authorization)?;
        let record = self.lookup_visible(credential, &q.name)?;
        Ok(self.describe(credential, &record))
    }
    pub fn list_repositories(
        &self,
        authorization: Option<&str>,
        q: p::ListRepositoriesRequest,
    ) -> Result<p::ListRepositoriesResponse, Status> {
        let credential = self.admin_credential(authorization)?;
        let limit = match q.limit {
            0 => 100,
            n @ 1..=1000 => n as usize,
            _ => return Err(Status::invalid_argument("list limit is 1..1000")),
        };
        if q.prefix.len() > kasu_core::MAX_REPO_NAME_BYTES
            || q.cursor.len() > kasu_core::MAX_REPO_NAME_BYTES
        {
            return Err(Status::invalid_argument("invalid list prefix or cursor"));
        }
        let store = self.registry.store();
        let prefix = q.prefix.to_ascii_lowercase();
        let mut cursor = q.cursor.to_ascii_lowercase();
        let mut repositories = Vec::new();
        // Callers with few grants scan in bounded pages; the cursor lets them
        // continue from where the scan stopped.
        let mut scanned = 0;
        loop {
            let names = store
                .list_repository_names(&prefix, &cursor, 256)
                .map_err(status)?;
            let exhausted = names.len() < 256;
            for entry in &names {
                scanned += 1;
                cursor = entry.name.clone();
                if let Some(record) = store.repository(&entry.key).map_err(status)?
                    && record.state != RepositoryState::Deleting
                    && self.visible(credential, &record)
                {
                    repositories.push(self.describe(credential, &record));
                }
                if repositories.len() >= limit || scanned >= 10_000 {
                    return Ok(p::ListRepositoriesResponse {
                        repositories,
                        cursor,
                    });
                }
            }
            if exhausted {
                return Ok(p::ListRepositoriesResponse {
                    repositories,
                    cursor: String::new(),
                });
            }
        }
    }
    pub fn update_repository(
        &self,
        authorization: Option<&str>,
        q: p::UpdateRepositoryRequest,
    ) -> Result<p::Repository, Status> {
        let credential = self.admin_credential(authorization)?;
        let actor = self.require_operator(credential)?;
        let mut record = self.lookup(&q.name)?;
        if record.config_version != q.expected_config_version {
            return Err(Status::aborted("repository configuration version mismatch"));
        }
        let configures = q.config_json.is_some() || q.default_branch.is_some();
        if configures && self.lifecycle.config_source == Some(ConfigSource::File) {
            return Err(Status::failed_precondition(
                "repository configuration is managed by KASU_REPOSITORIES_FILE",
            ));
        }
        let mut events = Vec::new();
        if let Some(config) = &q.config_json {
            record.config = RepositoryConfig::parse_request(config.as_bytes())?.encode();
            events.push("config");
        }
        if let Some(value) = &q.default_branch {
            record.default_branch = branch(value)?;
            events.push("default_branch");
        }
        if configures {
            record.config_version += 1;
        }
        if let Some(state) = q.state {
            record.state = match p::RepositoryState::try_from(state) {
                Ok(p::RepositoryState::Active) => RepositoryState::Active,
                Ok(p::RepositoryState::ReadOnly) => RepositoryState::ReadOnly,
                Ok(p::RepositoryState::Archived) => RepositoryState::Archived,
                Ok(p::RepositoryState::Deleting) => {
                    return Err(Status::invalid_argument(
                        "use DeleteRepository to delete a repository",
                    ));
                }
                Err(_) => return Err(Status::invalid_argument("unknown repository state")),
            };
            events.push(record.state.name());
        }
        if events.is_empty() {
            return Ok(self.describe(credential, &record));
        }
        let event = format!("update:{}", events.join(","));
        let record = self.written(
            self.registry
                .store()
                .update_repository(&record, &event, actor, now_seconds())
                .map_err(status)?,
        )?;
        Ok(self.describe(credential, &record))
    }
    pub fn rename_repository(
        &self,
        authorization: Option<&str>,
        q: p::RenameRepositoryRequest,
    ) -> Result<p::Repository, Status> {
        let credential = self.admin_credential(authorization)?;
        let actor = self.require_operator(credential)?;
        let record = self.lookup(&q.name)?;
        let new = name(&q.new_name)?;
        let now = now_seconds();
        let alias = match (q.keep_alias, self.lifecycle.alias_ttl) {
            (true, Some(ttl)) => {
                Some(now.saturating_add(ttl.as_secs().min(i64::MAX as u64) as i64))
            }
            _ => None,
        };
        let record = self.written(
            self.registry
                .store()
                .rename_repository(&record.key, record.version, &new, alias, actor, now)
                .map_err(status)?,
        )?;
        Ok(self.describe(credential, &record))
    }
    /// Stop all access and release the name; the mutable state and indexes
    /// are removed later by `purge_deleted_repositories`.
    pub fn delete_repository(
        &self,
        authorization: Option<&str>,
        q: p::DeleteRepositoryRequest,
    ) -> Result<p::Repository, Status> {
        let credential = self.admin_credential(authorization)?;
        let actor = self.require_operator(credential)?;
        let mut record = self.lookup(&q.name)?;
        record.state = RepositoryState::Deleting;
        record.deleted_at = now_seconds();
        let record = self.written(
            self.registry
                .store()
                .update_repository(&record, "deleted", actor, record.deleted_at)
                .map_err(status)?,
        )?;
        Ok(self.describe(credential, &record))
    }
    /// Remove refs, Land results, webhook state and the membership, Git OID and
    /// reachability indexes of deleted repositories, then their ref logs after
    /// the retention period. Shared immutable objects are not collected.
    pub fn purge_deleted_repositories(&self, now: i64) -> Result<usize, Status> {
        let store = self.registry.store();
        let mut purged = 0;
        let mut after = None;
        loop {
            let page = store
                .list_repositories(after.as_ref(), 256)
                .map_err(status)?;
            let Some(last) = page.last() else {
                return Ok(purged);
            };
            after = Some(last.key.clone());
            for mut record in page {
                if record.state != RepositoryState::Deleting || record.logs_purged {
                    continue;
                }
                if !record.purged {
                    store
                        .purge_repository_refs(&record.key, false)
                        .map_err(status)?;
                    let key = record.key.as_bytes();
                    for prefix in Repository::membership_prefixes(key)
                        .into_iter()
                        .chain(kasu_git::repository_index_prefixes(key))
                    {
                        while self
                            .meta
                            .delete_prefix(&prefix, PURGE_BATCH)
                            .map_err(status)?
                            > 0
                        {}
                    }
                    record.purged = true;
                    record = self.written(
                        store
                            .update_repository(&record, "purged", b"purge", now)
                            .map_err(status)?,
                    )?;
                    purged += 1;
                }
                let retention = self
                    .lifecycle
                    .ref_log_retention
                    .as_secs()
                    .min(i64::MAX as u64) as i64;
                if record.deleted_at.saturating_add(retention) <= now {
                    store
                        .purge_repository_refs(&record.key, true)
                        .map_err(status)?;
                    record.logs_purged = true;
                    self.written(
                        store
                            .update_repository(&record, "logs_purged", b"purge", now)
                            .map_err(status)?,
                    )?;
                }
            }
        }
    }
    /// Associate `target` and its history, trees, blobs and raw Git objects
    /// with `fork`. Every step is put-if-absent, so a retry resumes.
    fn fork_objects(
        &self,
        source: &RepositoryRecord,
        fork: &RepoKey,
        target: ChangesetId,
    ) -> Result<(), Status> {
        let ops = self.ops();
        let repo = &ops.ctx.repo;
        let from = source.key.as_bytes();
        let to = fork.as_bytes();
        let mut stack = vec![(target, false)];
        let mut seen = BTreeSet::new();
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                repo.associate_tree_graph(to, ops.root_tree(id).map_err(status)?)
                    .map_err(status)?;
                repo.associate_changeset(to, id).map_err(status)?;
                continue;
            }
            // Membership implies the whole history is already associated.
            if repo.require_membership(to, id).is_ok() || !seen.insert(id) {
                continue;
            }
            repo.require_membership(from, id).map_err(status)?;
            stack.push((id, true));
            for parent in repo.changeset(id).map_err(status)?.parents {
                stack.push((parent, false));
            }
        }
        let source_index = GitIndex {
            repo: Repository {
                meta: repo.meta,
                blobs: repo.blobs,
            },
            projection: ProjectionId::full(from),
        };
        let fork_index = GitIndex {
            repo: Repository {
                meta: repo.meta,
                blobs: repo.blobs,
            },
            projection: ProjectionId::full(to),
        };
        let tip = ops
            .engine
            .derive::<GitCommit>(&ops.ctx, target, &source_index.projection)
            .map_err(status)?
            .0;
        for oid in kasu_git::reachable(&source_index, [tip], &Default::default()).map_err(status)? {
            fork_index.copy_record(&source_index, oid).map_err(status)?;
        }
        // The synthesized commit depends only on changeset content, so the
        // fork advertises the same Git OIDs as its source.
        let forked = ops
            .engine
            .derive::<GitCommit>(&ops.ctx, target, &fork_index.projection)
            .map_err(status)?
            .0;
        if forked != tip {
            return Err(Status::internal(
                "forked Git commit differs from its source",
            ));
        }
        Ok(())
    }
    pub fn fork_repository(
        &self,
        authorization: Option<&str>,
        q: p::ForkRepositoryRequest,
    ) -> Result<p::Repository, Status> {
        let credential = self.admin_credential(authorization)?;
        let actor = self.require_operator(credential)?;
        let source = self.lookup(&q.source)?;
        kasu_git::validate_branch_name(&q.r#ref).map_err(status)?;
        let target = self
            .refs
            .resolve(source.key.as_bytes(), &q.r#ref)
            .map_err(status)?
            .target
            .ok_or_else(|| Status::not_found("source ref not found"))?;
        let new = name(&q.new_name)?;
        if q.idempotency_key.len() > 256 {
            return Err(Status::invalid_argument(
                "idempotency key exceeds 256 bytes",
            ));
        }
        let creation_key = fingerprint(&[
            b"kasu.admin.fork.v1",
            &q.idempotency_key,
            source.key.as_bytes(),
            &q.r#ref,
            &target.0,
            new.normalized().as_bytes(),
        ]);
        let now = now_seconds();
        let mut record =
            RepositoryRecord::new(RepoKey::generate().map_err(status)?, new, actor, now);
        record.default_branch = source.default_branch.clone();
        record.git_commit_rule = source.git_commit_rule;
        record.creation_key = creation_key.clone();
        let store = self.registry.store();
        let (record, replayed) = match store.create_repository(&record).map_err(status)? {
            RepositoryWrite::NameTaken if !q.idempotency_key.is_empty() => {
                let existing = self
                    .lookup(&q.new_name)
                    .map_err(|_| Status::already_exists("repository name is taken"))?;
                if existing.creation_key != creation_key {
                    return Err(Status::already_exists("repository name is taken"));
                }
                (existing, true)
            }
            write => (self.written(write)?, false),
        };
        self.fork_objects(&source, &record.key, target)?;
        let current = self
            .refs
            .resolve(record.key.as_bytes(), &q.r#ref)
            .map_err(status)?;
        if current.target.is_none() {
            self.refs
                .compare_and_swap(
                    record.key.as_bytes(),
                    &[RefUpdate {
                        name: q.r#ref.clone(),
                        expected: RefState::default(),
                        new: Some(target),
                    }],
                    actor,
                    now,
                )
                .map_err(status)?;
        }
        let mut response = self.describe(credential, &record);
        response.replayed = replayed;
        Ok(response)
    }
    /// Logical size of the objects reachable from the native refs, computed on
    /// demand and cached per ref state. Shared objects count in full.
    pub fn repository_usage(
        &self,
        authorization: Option<&str>,
        q: p::GetRepositoryRequest,
    ) -> Result<p::RepositoryUsage, Status> {
        let credential = self.admin_credential(authorization)?;
        let record = self.lookup_visible(credential, &q.name)?;
        let key = record.key.as_bytes();
        let ops = self.ops();
        let repo = &ops.ctx.repo;
        let mut tips: Vec<ChangesetId> = self
            .refs
            .list(key, b"")
            .map_err(status)?
            .into_iter()
            .filter_map(|(_, state)| state.target)
            .collect();
        tips.sort();
        tips.dedup();
        let mut cache_key = b"repo/usage/v1/".to_vec();
        let mut encoded = Encoder::default();
        encoded.bytes(key);
        for tip in &tips {
            encoded.raw(&tip.0);
        }
        cache_key.extend(ObjectId::hash(&encoded.0).0);
        if let Some(raw) = repo.meta.get(&cache_key).map_err(status)? {
            let mut decoder = kasu_core::Decoder::new(&raw);
            let mut next = || decoder.u64().map_err(status);
            return Ok(p::RepositoryUsage {
                name: record.name.to_string(),
                changesets: next()?,
                trees: next()?,
                blobs: next()?,
                blob_bytes: next()?,
                complete: true,
            });
        }
        let mut changesets = BTreeSet::new();
        let mut trees = BTreeSet::new();
        let mut blobs = BTreeSet::new();
        let mut blob_bytes = 0u64;
        let mut stack = tips;
        let mut complete = true;
        'walk: while let Some(id) = stack.pop() {
            if !changesets.insert(id) {
                continue;
            }
            stack.extend(repo.changeset(id).map_err(status)?.parents);
            let mut pending = vec![ops.root_tree(id).map_err(status)?];
            while let Some(tree) = pending.pop() {
                if !trees.insert(tree) {
                    continue;
                }
                for entry in repo.tree(tree).map_err(status)?.entries.values() {
                    match entry {
                        TreeEntry::SubTree(child) => pending.push(*child),
                        TreeEntry::Leaf(leaf) => {
                            if let Some(blob) = leaf.blob()
                                && blobs.insert(blob)
                            {
                                blob_bytes += repo.blob_metadata(blob).map_err(status)?.size;
                            }
                        }
                    }
                }
                if changesets.len() + trees.len() + blobs.len() > MAX_USAGE_OBJECTS {
                    complete = false;
                    break 'walk;
                }
            }
        }
        let usage = p::RepositoryUsage {
            name: record.name.to_string(),
            changesets: changesets.len() as u64,
            trees: trees.len() as u64,
            blobs: blobs.len() as u64,
            blob_bytes,
            complete,
        };
        if complete {
            let mut encoded = Encoder::default();
            for value in [usage.changesets, usage.trees, usage.blobs, usage.blob_bytes] {
                encoded.u64(value);
            }
            repo.meta
                .put_if_absent(&cache_key, &encoded.0)
                .map_err(status)?;
        }
        Ok(usage)
    }
    /// Periodically purge deleted repositories until shutdown.
    pub async fn purge_loop(
        self: Arc<Self>,
        interval: Duration,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            let backend = self.clone();
            match crate::admission::blocking(move || {
                backend.purge_deleted_repositories(now_seconds())
            })
            .await
            {
                Ok(Ok(0)) => {}
                Ok(Ok(count)) => tracing::info!(count, "deleted repositories purged"),
                Ok(Err(error)) => {
                    tracing::warn!(message=%error.message(),"repository purge will retry")
                }
                Err(_) => tracing::error!("repository purge worker failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => return,
            }
        }
    }
}

#[derive(Clone)]
pub struct AdminService {
    state: Arc<Backend>,
}
impl AdminService {
    pub fn new(state: Arc<Backend>) -> Self {
        Self { state }
    }
    pub fn into_server(self) -> crate::ObservedService<AdminServer<Self>> {
        use tower::Layer;
        let layer = crate::ServerLayer::grpc(self.state.clone());
        layer.layer(AdminServer::new(self).max_decoding_message_size(2 * 1024 * 1024))
    }
}
macro_rules! admin {
    ($(($method:ident,$backend:ident,$request:ident,$response:ident,$path:literal)),* $(,)?) => {
        #[tonic::async_trait]
        impl Admin for AdminService {
            $(async fn $method(&self, request: Request<p::$request>) -> Result<Response<p::$response>, Status> {
                let authorization = request.metadata().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
                let input = request.into_inner();
                let state = self.state.clone();
                let result = crate::admission::blocking(move || state.$backend(authorization.as_deref(), input)).await.map_err(|_| Status::internal("admin worker failed"))??;
                Ok(Response::new(result))
            })*
        }
        $(async fn $method(State(state): State<Arc<Backend>>, headers: HeaderMap, Json(input): Json<p::$request>) -> Result<Json<p::$response>, crate::transport::HttpError> {
            let authorization = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let result = crate::admission::blocking(move || state.$backend(authorization.as_deref(), input)).await.map_err(|_| crate::transport::HttpError(Status::internal("admin worker failed")))?.map_err(crate::transport::HttpError)?;
            Ok(Json(result))
        })*
        pub fn admin_http_router(state: Arc<Backend>) -> Router {
            Router::new()$(.route($path, post($method)))*.layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024)).with_state(state)
        }
    };
}
admin! {
    (create_repository, create_repository, CreateRepositoryRequest, Repository, "/v1/admin/create-repository"),
    (get_repository, get_repository, GetRepositoryRequest, Repository, "/v1/admin/get-repository"),
    (list_repositories, list_repositories, ListRepositoriesRequest, ListRepositoriesResponse, "/v1/admin/list-repositories"),
    (update_repository, update_repository, UpdateRepositoryRequest, Repository, "/v1/admin/update-repository"),
    (rename_repository, rename_repository, RenameRepositoryRequest, Repository, "/v1/admin/rename-repository"),
    (delete_repository, delete_repository, DeleteRepositoryRequest, Repository, "/v1/admin/delete-repository"),
    (fork_repository, fork_repository, ForkRepositoryRequest, Repository, "/v1/admin/fork-repository"),
    (get_repository_usage, repository_usage, GetRepositoryRequest, RepositoryUsage, "/v1/admin/repository-usage"),
}
