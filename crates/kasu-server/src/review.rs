use crate::{
    Backend, Mode,
    auth::{Credential, not_found},
    backend::status,
    registry::now_seconds,
};
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use kasu_core::{self as c, Canonical, ChangesetId, Encoder, ObjectId};
use kasu_proto::v1::{
    self as p,
    review_server::{Review, ReviewServer},
};
use kasu_store::{
    REVIEW_CLOSED, REVIEW_OPEN, REVIEW_SUBMITTED, RefState, RefUpdate, ReviewCommentRow,
    ReviewFilter, ReviewRefUpdate, ReviewRequestRecord, ReviewRevisionRow, ReviewRow, ReviewStore,
    ReviewVoteRow, ReviewWrite, ReviewWriteResult,
};
use prost::Message;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tonic::{Request, Response, Status};

const MAX_STACK: usize = 64;
const MAX_REVISIONS: usize = 256;
const MAX_COMMENTS: usize = 10_000;
const MAX_TEXT: usize = 64 * 1024;
const MAX_TITLE: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubmitMethodName {
    Rebase,
    Squash,
}
impl SubmitMethodName {
    fn code(self) -> p::SubmitMethod {
        match self {
            Self::Rebase => p::SubmitMethod::Rebase,
            Self::Squash => p::SubmitMethod::Squash,
        }
    }
}
fn one() -> u32 {
    1
}
fn yes() -> bool {
    true
}
fn both() -> Vec<SubmitMethodName> {
    vec![SubmitMethodName::Rebase, SubmitMethodName::Squash]
}
/// Protected refs of a repository: updates go through an approved review.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewPolicy {
    /// The first matching rule applies.
    pub rules: Vec<ReviewRule>,
    /// Account name globs that may update protected refs directly.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bypass_accounts: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRule {
    pub ref_patterns: Vec<String>,
    #[serde(default = "one")]
    pub required_approvals: u32,
    /// Account name globs; None means every account that can write the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvers: Option<Vec<String>>,
    #[serde(default)]
    pub allow_self_approval: bool,
    #[serde(default = "yes")]
    pub block_on_request_changes: bool,
    #[serde(default = "yes")]
    pub keep_approvals_on_trivial_rebase: bool,
    #[serde(default)]
    pub require_resolved_threads: bool,
    #[serde(default = "both")]
    pub methods: Vec<SubmitMethodName>,
    /// Append `Review: <repo>#<number>` to landed messages.
    #[serde(default = "yes")]
    pub trailer: bool,
    /// Path owners (CODEOWNERS-like): changes below `paths` also need
    /// approvals from the matching `approvers`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<PathOwners>,
}
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathOwners {
    /// Path globs; a pattern ending in `/` owns everything below it.
    pub paths: Vec<String>,
    pub approvers: Vec<String>,
    #[serde(default = "one")]
    pub required_approvals: u32,
}
impl PathOwners {
    pub fn owns(&self, path: &[u8]) -> bool {
        self.paths
            .iter()
            .any(|pattern| match pattern.strip_suffix('/') {
                Some(dir) => path.starts_with(dir.as_bytes()) && path.get(dir.len()) == Some(&b'/'),
                None => crate::write::glob(pattern.as_bytes(), path),
            })
    }
}
impl Default for ReviewRule {
    fn default() -> Self {
        Self {
            ref_patterns: Vec::new(),
            required_approvals: 1,
            approvers: None,
            allow_self_approval: false,
            block_on_request_changes: true,
            keep_approvals_on_trivial_rebase: true,
            require_resolved_threads: false,
            methods: both(),
            trailer: true,
            owners: Vec::new(),
        }
    }
}
impl ReviewRule {
    pub fn matches(&self, name: &[u8]) -> bool {
        self.ref_patterns
            .iter()
            .any(|p| crate::write::glob(p.as_bytes(), name))
    }
}
impl ReviewPolicy {
    pub fn validate(&self) -> Result<(), Status> {
        let invalid = || Status::invalid_argument("invalid review policy");
        if self.rules.len() > 32 || self.bypass_accounts.len() > 128 {
            return Err(invalid());
        }
        for rule in &self.rules {
            if rule.ref_patterns.is_empty()
                || rule.ref_patterns.len() > 32
                || rule
                    .ref_patterns
                    .iter()
                    .any(|p| p.is_empty() || p.len() > 4096 || !p.starts_with("refs/"))
                || rule.required_approvals > 10
                || rule.methods.is_empty()
                || rule
                    .approvers
                    .as_ref()
                    .is_some_and(|a| a.len() > 128 || a.iter().any(|g| g.is_empty()))
                || rule.owners.len() > 64
                || rule.owners.iter().any(|o| {
                    o.paths.is_empty()
                        || o.paths.len() > 64
                        || o.paths.iter().any(|p| p.is_empty() || p.len() > 4096)
                        || o.approvers.is_empty()
                        || o.approvers.len() > 128
                        || !(1..=10).contains(&o.required_approvals)
                })
            {
                return Err(invalid());
            }
        }
        if self
            .bypass_accounts
            .iter()
            .any(|g| g.is_empty() || g.len() > 128)
        {
            return Err(invalid());
        }
        Ok(())
    }
}

/// A review with its revisions and votes, as stored.
struct Loaded {
    version: u64,
    record: p::ReviewRecord,
}
impl Loaded {
    fn latest(&self) -> &p::ReviewRevision {
        self.record
            .revisions
            .last()
            .expect("a review has at least one revision")
    }
    fn state(&self) -> p::ReviewState {
        p::ReviewState::try_from(self.record.state).unwrap_or(p::ReviewState::Open)
    }
}
fn state_code(state: p::ReviewState) -> u8 {
    match state {
        p::ReviewState::Open => REVIEW_OPEN,
        p::ReviewState::Submitted => REVIEW_SUBMITTED,
        p::ReviewState::Closed => REVIEW_CLOSED,
    }
}
fn header_data(record: &p::ReviewRecord) -> Vec<u8> {
    p::ReviewRecord {
        title: record.title.clone(),
        description: record.description.clone(),
        created_at: record.created_at,
        updated_at: record.updated_at,
        submitted: record.submitted.clone(),
        ..Default::default()
    }
    .encode_to_vec()
}
fn decode<T: Message + Default>(bytes: &[u8]) -> Result<T, Status> {
    T::decode(bytes).map_err(|_| Status::internal("corrupt review record"))
}
fn event(kind: &str, actor: &str, detail: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"kind": kind, "actor": actor, "detail": detail}))
        .expect("review events serialize")
}
fn conflict_status(write: ReviewWriteResult) -> Status {
    match write {
        ReviewWriteResult::Duplicate(n) => {
            Status::already_exists(format!("open review #{n} has the same target and head"))
        }
        ReviewWriteResult::VersionMismatch => {
            Status::aborted("review changed concurrently; reload and retry")
        }
        _ => Status::internal("unexpected review write result"),
    }
}
fn text(bytes: &[u8], limit: usize, what: &str) -> Result<(), Status> {
    if bytes.len() > limit {
        return Err(Status::invalid_argument(format!(
            "{what} exceeds {limit} bytes"
        )));
    }
    Ok(())
}

impl Backend {
    pub fn with_reviews(mut self, store: Arc<dyn ReviewStore>) -> Self {
        self.reviews = Some(store);
        self
    }
    pub(crate) fn review_policy(&self) -> Option<&ReviewPolicy> {
        self.repo.as_ref()?.repo.config.review.as_ref()
    }
    /// The rule protecting `name`, if any.
    pub(crate) fn review_rule(&self, name: &[u8]) -> Option<&ReviewRule> {
        self.review_policy()?.rules.iter().find(|r| r.matches(name))
    }
    fn principal_account(&self) -> Option<&crate::RepositoryAccount> {
        self.principal
            .and_then(Credential::account)
            .map(|a| &self.repository_accounts[a])
    }
    /// Whether the caller may update protected refs without a review.
    pub(crate) fn review_bypass(&self) -> bool {
        match self.principal {
            None | Some(Credential::Anonymous | Credential::Operator) => true,
            Some(credential) if credential.anonymous_delegation() => false,
            Some(_) => self.principal_account().is_some_and(|account| {
                self.review_policy().is_some_and(|policy| {
                    policy
                        .bypass_accounts
                        .iter()
                        .any(|g| crate::write::glob(g.as_bytes(), account.name.as_bytes()))
                })
            }),
        }
    }
    /// Refuse a direct update of a protected ref.
    pub(crate) fn review_gate(&self, name: &[u8]) -> Option<&'static str> {
        (self.review_rule(name).is_some() && !self.review_bypass()).then_some("review required")
    }
    /// The ref_log actor of a direct update of `names`, marking bypasses.
    pub(crate) fn ref_actor<'a>(
        &self,
        fallback: &[u8],
        mut names: impl Iterator<Item = &'a [u8]>,
    ) -> Vec<u8> {
        let actor = self.write_actor(fallback);
        if !names.any(|n| self.review_rule(n).is_some()) {
            return actor;
        }
        let principal = self
            .write_principal()
            .map(str::as_bytes)
            .unwrap_or(fallback);
        let mut via = self.via.as_deref().cloned().unwrap_or_default();
        if !via.is_empty() {
            via.push(b';');
        }
        via.extend_from_slice(b"bypass");
        kasu_store::delegated_actor(principal, &via)
    }
    fn review_store(&self) -> Result<&Arc<dyn ReviewStore>, Status> {
        self.reviews
            .as_ref()
            .ok_or_else(|| Status::unimplemented("reviews are not enabled on this server"))
    }
    /// Scope a review request. Directory-restricted accounts cannot see
    /// reviews, whose titles, comments and anchors could leak other paths.
    fn review_scope(
        &self,
        authorization: Option<&str>,
        repo: &[u8],
        mode: Mode,
    ) -> Result<Self, Status> {
        self.review_store()?;
        let scoped = self.scope(authorization, repo, mode)?;
        if scoped.read_scope.is_some() {
            return Err(not_found());
        }
        Ok(scoped)
    }
    /// The account name reviews attribute the caller's actions to.
    fn review_actor(&self) -> Result<String, Status> {
        match self.principal {
            Some(credential) if credential.anonymous_delegation() => Err(
                Status::permission_denied("anonymous visitors cannot take part in reviews"),
            ),
            None | Some(Credential::Operator) => Ok("operator".into()),
            Some(Credential::Anonymous) => Ok("anonymous".into()),
            Some(_) => self
                .principal_account()
                .map(|a| a.name.clone())
                .ok_or_else(|| Status::permission_denied("reviews require an account")),
        }
    }
    fn review_admin(&self) -> bool {
        self.principal
            .is_none_or(|credential| self.administers(credential, self.record()))
    }
    fn load_review(&self, number: u64) -> Result<Loaded, Status> {
        let store = self.review_store()?;
        let key = self.key();
        let row = store
            .review(key, number)
            .map_err(status)?
            .ok_or_else(|| Status::not_found("review not found"))?;
        let mut record: p::ReviewRecord = decode(&row.data)?;
        record.repo = self.context_requested().to_vec();
        record.number = row.number;
        record.version = row.version;
        record.state = match row.state {
            REVIEW_SUBMITTED => p::ReviewState::Submitted,
            REVIEW_CLOSED => p::ReviewState::Closed,
            _ => p::ReviewState::Open,
        } as i32;
        record.target = row.target;
        record.author = row.author;
        record.revisions = store
            .review_revisions(key, number)
            .map_err(status)?
            .iter()
            .map(|r| decode(&r.data))
            .collect::<Result<_, _>>()?;
        record.votes = store
            .review_votes(key, number)
            .map_err(status)?
            .iter()
            .map(|v| decode(&v.data))
            .collect::<Result<_, _>>()?;
        if record.revisions.is_empty() {
            return Err(Status::internal("review has no revision"));
        }
        Ok(Loaded {
            version: row.version,
            record,
        })
    }
    fn context_requested(&self) -> &[u8] {
        &self.repo.as_ref().expect("a scoped backend").requested
    }
    /// A write of `loaded` with `record` as its new header.
    fn review_write(
        &self,
        loaded: &Loaded,
        record: &p::ReviewRecord,
        event: Vec<u8>,
    ) -> ReviewWrite {
        let head = record
            .revisions
            .last()
            .and_then(|r| r.changesets.last())
            .cloned()
            .unwrap_or_default();
        ReviewWrite {
            review: ReviewRow {
                number: record.number,
                version: loaded.version + 1,
                state: state_code(
                    p::ReviewState::try_from(record.state).unwrap_or(p::ReviewState::Open),
                ),
                target: record.target.clone(),
                head,
                author: record.author.clone(),
                data: header_data(record),
            },
            expected_version: loaded.version,
            revision: None,
            votes: Vec::new(),
            comments: Vec::new(),
            event,
            request: None,
            ref_update: None,
            time: now_seconds(),
        }
    }
    /// Validate a stack: members of this repository whose first parents
    /// chain. Returns the base (the first changeset's first parent).
    fn review_stack(
        &self,
        raw: &[Vec<u8>],
    ) -> Result<(Option<ChangesetId>, Vec<ChangesetId>), Status> {
        if raw.is_empty() || raw.len() > MAX_STACK {
            return Err(Status::invalid_argument(format!(
                "a review stack has 1..{MAX_STACK} changesets"
            )));
        }
        let repo = self.context_requested().to_vec();
        let ids = raw
            .iter()
            .map(|id| self.id(&repo, id))
            .collect::<Result<Vec<_>, _>>()?;
        let repository = self.ops().ctx.repo;
        let mut base = None;
        for (i, id) in ids.iter().enumerate() {
            let parent = repository
                .changeset(*id)
                .map_err(status)?
                .parents
                .first()
                .copied();
            if i == 0 {
                base = parent;
            } else if parent != Some(ids[i - 1]) {
                return Err(Status::invalid_argument(
                    "each changeset's first parent must be the previous one in the stack",
                ));
            }
        }
        Ok((base, ids))
    }
    fn tree_of(&self, id: Option<ChangesetId>) -> Result<c::TreeId, Status> {
        let ops = self.ops();
        match id {
            Some(id) => ops.root_tree(id).map_err(status),
            None => ops.ctx.repo.put_tree(&c::Tree::default()).map_err(status),
        }
    }
    /// The changes of each stack member against its first parent, by path
    /// and resulting entry. Equal across a rebase that changes nothing.
    pub(crate) fn review_fingerprint(&self, stack: &[ChangesetId]) -> Result<Vec<u8>, Status> {
        let repository = self.ops().ctx.repo;
        let mut encoded = Encoder::default();
        encoded.raw(b"kasu.review.fingerprint.v1");
        for id in stack {
            let parent = repository
                .changeset(*id)
                .map_err(status)?
                .parents
                .first()
                .copied();
            let changes =
                kasu_derived::diff(&repository, self.tree_of(parent)?, self.tree_of(Some(*id))?)
                    .map_err(status)?;
            encoded.u64(changes.len() as u64);
            for change in changes {
                encoded.raw(&change.encode());
            }
        }
        Ok(ObjectId::hash(&encoded.0).0.to_vec())
    }
    fn revision(
        &self,
        number: u32,
        base: Option<ChangesetId>,
        stack: &[ChangesetId],
        actor: &str,
    ) -> Result<p::ReviewRevision, Status> {
        Ok(p::ReviewRevision {
            number,
            base: base.map(|id| id.0.to_vec()).unwrap_or_default(),
            changesets: stack.iter().map(|id| id.0.to_vec()).collect(),
            fingerprint: self.review_fingerprint(stack)?,
            created_by: actor.into(),
            created_at: now_seconds(),
        })
    }
    /// Writing to `target`, ignoring the review requirement itself.
    fn require_target_write(&self, target: &[u8]) -> Result<(), Status> {
        kasu_git::validate_branch_name(target).map_err(status)?;
        if !self.allows_ref(target) {
            return Err(Status::permission_denied("the target ref is not writable"));
        }
        Ok(())
    }

    pub fn create_review(
        &self,
        authorization: Option<&str>,
        q: p::CreateReviewRequest,
    ) -> Result<p::ReviewRecord, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Write)?;
        scoped.create_review_scoped(q)
    }
    fn create_review_scoped(&self, q: p::CreateReviewRequest) -> Result<p::ReviewRecord, Status> {
        let actor = self.review_actor()?;
        self.require_target_write(&q.target)?;
        if q.title.is_empty() {
            return Err(Status::invalid_argument("a review needs a title"));
        }
        text(&q.title, MAX_TITLE, "title")?;
        text(&q.description, MAX_TEXT, "description")?;
        if q.idempotency_key.len() > 256 {
            return Err(Status::invalid_argument(
                "idempotency key exceeds 256 bytes",
            ));
        }
        let (base, stack) = self.review_stack(&q.changesets)?;
        let now = now_seconds();
        let record = p::ReviewRecord {
            state: p::ReviewState::Open as i32,
            target: q.target.clone(),
            title: q.title.clone(),
            description: q.description.clone(),
            author: actor.clone(),
            created_at: now,
            updated_at: now,
            revisions: vec![self.revision(1, base, &stack, &actor)?],
            ..Default::default()
        };
        let initial = Loaded {
            version: 0,
            record: record.clone(),
        };
        let mut write = self.review_write(
            &initial,
            &record,
            event("create", &actor, serde_json::json!({})),
        );
        write.review.number = 0;
        write.revision = Some(ReviewRevisionRow {
            revision: 1,
            data: record.revisions[0].encode_to_vec(),
        });
        if !q.idempotency_key.is_empty() {
            let mut fingerprint = Encoder::default();
            fingerprint.raw(b"kasu.review.create.v1");
            for part in [&q.target, &q.title, &q.description] {
                fingerprint.bytes(part);
            }
            for id in &q.changesets {
                fingerprint.bytes(id);
            }
            write.request = Some(ReviewRequestRecord {
                account: actor,
                key: [b"create\0".as_slice(), &q.idempotency_key].concat(),
                fingerprint: ObjectId::hash(&fingerprint.0).0.to_vec(),
                response: Vec::new(),
            });
        }
        match self
            .review_store()?
            .write_review(self.key(), &write)
            .map_err(status)?
        {
            ReviewWriteResult::Applied { number } => Ok(self.load_review(number)?.record),
            ReviewWriteResult::Replayed { response } => {
                let number = u64::from_be_bytes(
                    response
                        .try_into()
                        .map_err(|_| Status::internal("corrupt review request record"))?,
                );
                let mut record = self.load_review(number)?.record;
                record.replayed = true;
                Ok(record)
            }
            other => Err(conflict_status(other)),
        }
    }
    pub fn get_review(
        &self,
        authorization: Option<&str>,
        q: p::GetReviewRequest,
    ) -> Result<p::ReviewRecord, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        Ok(scoped.load_review(q.number)?.record)
    }
    pub fn list_reviews(
        &self,
        authorization: Option<&str>,
        q: p::ListReviewsRequest,
    ) -> Result<p::ListReviewsResponse, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        let limit = match q.limit {
            0 => 50,
            n @ 1..=500 => n as usize,
            _ => return Err(Status::invalid_argument("list limit is 1..500")),
        };
        let filter = ReviewFilter {
            state: q
                .state
                .map(|s| p::ReviewState::try_from(s).map(state_code))
                .transpose()
                .map_err(|_| Status::invalid_argument("unknown review state"))?,
            target: (!q.target.is_empty()).then_some(q.target),
            author: (!q.author.is_empty()).then_some(q.author),
        };
        let rows = scoped
            .review_store()?
            .list_reviews(scoped.key(), &filter, q.cursor, limit)
            .map_err(status)?;
        let cursor = if rows.len() == limit {
            rows.last().map(|r| r.number).unwrap_or_default()
        } else {
            0
        };
        Ok(p::ListReviewsResponse {
            reviews: rows
                .iter()
                .map(|row| scoped.load_review(row.number).map(|l| l.record))
                .collect::<Result<_, _>>()?,
            cursor,
        })
    }
    pub fn update_review(
        &self,
        authorization: Option<&str>,
        q: p::UpdateReviewRequest,
    ) -> Result<p::ReviewRecord, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        let actor = scoped.review_actor()?;
        let loaded = scoped.load_review(q.number)?;
        if loaded.version != q.expected_version {
            return Err(Status::aborted("review version mismatch"));
        }
        if loaded.record.author != actor && !scoped.review_admin() {
            return Err(Status::permission_denied(
                "only the author or a repository administrator can change a review",
            ));
        }
        let mut record = loaded.record.clone();
        let mut changes = Vec::new();
        if let Some(title) = q.title {
            if title.is_empty() {
                return Err(Status::invalid_argument("a review needs a title"));
            }
            text(&title, MAX_TITLE, "title")?;
            record.title = title;
            changes.push("title");
        }
        if let Some(description) = q.description {
            text(&description, MAX_TEXT, "description")?;
            record.description = description;
            changes.push("description");
        }
        if let Some(state) = q.state {
            let state = p::ReviewState::try_from(state)
                .map_err(|_| Status::invalid_argument("unknown review state"))?;
            match (loaded.state(), state) {
                (current, next) if current == next => {}
                (p::ReviewState::Open, p::ReviewState::Closed) => changes.push("close"),
                (p::ReviewState::Closed, p::ReviewState::Open) => changes.push("reopen"),
                _ => {
                    return Err(Status::failed_precondition(
                        "submitted reviews cannot change state; use Submit to land",
                    ));
                }
            }
            record.state = state as i32;
        }
        if changes.is_empty() {
            return Ok(loaded.record);
        }
        record.updated_at = now_seconds();
        let write = scoped.review_write(
            &loaded,
            &record,
            event("update", &actor, serde_json::json!({ "fields": changes })),
        );
        match scoped
            .review_store()?
            .write_review(scoped.key(), &write)
            .map_err(status)?
        {
            ReviewWriteResult::Applied { number } => Ok(scoped.load_review(number)?.record),
            other => Err(conflict_status(other)),
        }
    }
    fn append_revision(
        &self,
        loaded: &Loaded,
        expected_revision: u32,
        base: Option<ChangesetId>,
        stack: &[ChangesetId],
        actor: &str,
        kind: &str,
    ) -> Result<p::ReviewRecord, Status> {
        if loaded.state() != p::ReviewState::Open {
            return Err(Status::failed_precondition("the review is not open"));
        }
        let latest = loaded.latest().number;
        if latest != expected_revision {
            return Err(Status::aborted(format!(
                "the latest revision is {latest}, not {expected_revision}"
            )));
        }
        if loaded.record.revisions.len() >= MAX_REVISIONS {
            return Err(Status::resource_exhausted(
                "a review has at most 256 revisions",
            ));
        }
        let mut record = loaded.record.clone();
        let revision = self.revision(latest + 1, base, stack, actor)?;
        record.revisions.push(revision.clone());
        record.updated_at = now_seconds();
        let mut write = self.review_write(
            loaded,
            &record,
            event(
                kind,
                actor,
                serde_json::json!({ "revision": revision.number }),
            ),
        );
        write.revision = Some(ReviewRevisionRow {
            revision: revision.number,
            data: revision.encode_to_vec(),
        });
        match self
            .review_store()?
            .write_review(self.key(), &write)
            .map_err(status)?
        {
            ReviewWriteResult::Applied { number } => Ok(self.load_review(number)?.record),
            other => Err(conflict_status(other)),
        }
    }
    pub fn add_revision(
        &self,
        authorization: Option<&str>,
        q: p::AddRevisionRequest,
    ) -> Result<p::ReviewRecord, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Write)?;
        let actor = scoped.review_actor()?;
        let loaded = scoped.load_review(q.number)?;
        scoped.require_target_write(&loaded.record.target)?;
        let (base, stack) = scoped.review_stack(&q.changesets)?;
        scoped.append_revision(
            &loaded,
            q.expected_revision,
            base,
            &stack,
            &actor,
            "revision",
        )
    }
    pub fn rebase_review(
        &self,
        authorization: Option<&str>,
        q: p::RebaseReviewRequest,
    ) -> Result<p::RebaseReviewResponse, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Write)?;
        let actor = scoped.review_actor()?;
        let loaded = scoped.load_review(q.number)?;
        let target = loaded.record.target.clone();
        scoped.require_target_write(&target)?;
        let repo = scoped.context_requested().to_vec();
        let onto = if q.onto.is_empty() {
            scoped
                .refs
                .resolve(scoped.key(), &target)
                .map_err(status)?
                .target
                .ok_or_else(|| Status::failed_precondition("the target ref does not exist"))?
        } else {
            scoped.id(&repo, &q.onto)?
        };
        let latest = loaded.latest();
        let stack = latest
            .changesets
            .iter()
            .map(|id| scoped.id(&repo, id))
            .collect::<Result<Vec<_>, _>>()?;
        if latest.base == onto.0 {
            return Ok(p::RebaseReviewResponse {
                review: Some(loaded.record),
                ..Default::default()
            });
        }
        let mut rebased = Vec::with_capacity(stack.len());
        let mut parent = onto;
        for (index, id) in stack.iter().enumerate() {
            let committer = scoped
                .ops()
                .ctx
                .repo
                .changeset(*id)
                .map_err(status)?
                .committer;
            match scoped
                .ops()
                .rebase(*id, parent, committer)
                .map_err(status)?
            {
                kasu_ops::RewriteResult::Created { id, .. } => {
                    scoped.created(id)?;
                    rebased.push(id);
                    parent = id;
                }
                conflict => {
                    return Ok(p::RebaseReviewResponse {
                        review: Some(loaded.record),
                        conflict: Some(scoped.rewritten(conflict)?),
                        conflict_index: index as u32,
                    });
                }
            }
        }
        let record = scoped.append_revision(
            &loaded,
            q.expected_revision,
            Some(onto),
            &rebased,
            &actor,
            "rebase",
        )?;
        Ok(p::RebaseReviewResponse {
            review: Some(record),
            ..Default::default()
        })
    }

    fn comments(&self, number: u64) -> Result<Vec<p::ReviewComment>, Status> {
        self.review_store()?
            .review_comments(self.key(), number)
            .map_err(status)?
            .iter()
            .map(|row| decode(&row.data))
            .collect()
    }
    /// Apply `change` to the latest state of a review, retrying when another
    /// writer changed it meanwhile.
    fn mutate_review(
        &self,
        number: u64,
        mut change: impl FnMut(&Loaded, &[p::ReviewComment]) -> Result<ReviewWrite, Status>,
    ) -> Result<u64, Status> {
        for _ in 0..8 {
            let loaded = self.load_review(number)?;
            let comments = self.comments(number)?;
            let write = change(&loaded, &comments)?;
            match self
                .review_store()?
                .write_review(self.key(), &write)
                .map_err(status)?
            {
                ReviewWriteResult::Applied { number } => return Ok(number),
                ReviewWriteResult::VersionMismatch => continue,
                other => return Err(conflict_status(other)),
            }
        }
        Err(Status::aborted("review is busy; retry"))
    }
    fn comment_row(comment: &p::ReviewComment) -> ReviewCommentRow {
        ReviewCommentRow {
            id: comment.id,
            author: comment.author.clone(),
            draft: comment.draft,
            data: comment.encode_to_vec(),
        }
    }
    pub fn list_comments(
        &self,
        authorization: Option<&str>,
        q: p::ListCommentsRequest,
    ) -> Result<p::ListCommentsResponse, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        scoped.load_review(q.number)?;
        let actor = scoped.review_actor().ok();
        Ok(p::ListCommentsResponse {
            comments: scoped
                .comments(q.number)?
                .into_iter()
                .filter(|c| !c.draft || Some(&c.author) == actor.as_ref())
                .collect(),
        })
    }
    fn validate_anchor(loaded: &Loaded, revision: u32, anchor: &p::Anchor) -> Result<(), Status> {
        let revision = loaded
            .record
            .revisions
            .iter()
            .find(|r| r.number == revision)
            .ok_or_else(|| Status::invalid_argument("unknown revision"))?;
        c::RepoPath::new(anchor.path.clone()).map_err(status)?;
        if anchor.line == 0
            || anchor
                .changeset_index
                .is_some_and(|i| i as usize >= revision.changesets.len())
            || p::AnchorSide::try_from(anchor.side).is_err()
        {
            return Err(Status::invalid_argument("invalid comment anchor"));
        }
        Ok(())
    }
    pub fn save_draft(
        &self,
        authorization: Option<&str>,
        q: p::SaveDraftRequest,
    ) -> Result<p::ReviewComment, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        let actor = scoped.review_actor()?;
        if q.body.is_empty() {
            return Err(Status::invalid_argument("a comment needs a body"));
        }
        text(&q.body, MAX_TEXT, "comment")?;
        let mut saved = None;
        scoped.mutate_review(q.number, |loaded, comments| {
            if !loaded
                .record
                .revisions
                .iter()
                .any(|r| r.number == q.revision)
            {
                return Err(Status::invalid_argument("unknown revision"));
            }
            if comments.len() >= MAX_COMMENTS {
                return Err(Status::resource_exhausted(
                    "a review has at most 10000 comments",
                ));
            }
            let anchor = match q.parent {
                Some(parent) => {
                    let root = comments
                        .iter()
                        .find(|c| c.id == parent && !c.draft)
                        .ok_or_else(|| Status::invalid_argument("unknown parent comment"))?;
                    if root.parent.is_some() {
                        return Err(Status::invalid_argument("reply to the root of a thread"));
                    }
                    None
                }
                None => {
                    if let Some(anchor) = &q.anchor {
                        Self::validate_anchor(loaded, q.revision, anchor)?;
                    }
                    q.anchor.clone()
                }
            };
            let now = now_seconds();
            let comment = p::ReviewComment {
                id: comments.iter().map(|c| c.id).max().unwrap_or(0) + 1,
                parent: q.parent,
                revision: q.revision,
                anchor,
                author: actor.clone(),
                body: q.body.clone(),
                draft: true,
                resolved: false,
                created_at: now,
                edited_at: now,
                deleted: false,
            };
            let mut write = scoped.review_write(loaded, &loaded.record, Vec::new());
            write.comments.push(Self::comment_row(&comment));
            saved = Some(comment);
            Ok(write)
        })?;
        saved.ok_or_else(|| Status::internal("draft was not saved"))
    }
    pub fn edit_comment(
        &self,
        authorization: Option<&str>,
        q: p::EditCommentRequest,
    ) -> Result<p::ReviewComment, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        let actor = scoped.review_actor()?;
        if !q.delete && q.body.is_empty() {
            return Err(Status::invalid_argument("a comment needs a body"));
        }
        text(&q.body, MAX_TEXT, "comment")?;
        let mut saved = None;
        scoped.mutate_review(q.number, |loaded, comments| {
            let mut comment = comments
                .iter()
                .find(|c| c.id == q.id && !c.deleted && (!c.draft || c.author == actor))
                .cloned()
                .ok_or_else(|| Status::not_found("comment not found"))?;
            if comment.author != actor {
                return Err(Status::permission_denied("only the author edits a comment"));
            }
            if q.delete {
                comment.deleted = true;
                comment.body.clear();
            } else {
                comment.body = q.body.clone();
            }
            comment.edited_at = now_seconds();
            let mut write = scoped.review_write(loaded, &loaded.record, Vec::new());
            write.comments.push(Self::comment_row(&comment));
            saved = Some(comment);
            Ok(write)
        })?;
        saved.ok_or_else(|| Status::internal("comment was not saved"))
    }
    pub fn resolve_thread(
        &self,
        authorization: Option<&str>,
        q: p::ResolveThreadRequest,
    ) -> Result<p::ReviewComment, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        let actor = scoped.review_actor()?;
        let mut saved = None;
        scoped.mutate_review(q.number, |loaded, comments| {
            let mut comment = comments
                .iter()
                .find(|c| c.id == q.id && !c.draft)
                .cloned()
                .ok_or_else(|| Status::not_found("comment not found"))?;
            if comment.parent.is_some() {
                return Err(Status::invalid_argument("only a thread root is resolved"));
            }
            comment.resolved = q.resolved;
            let mut write = scoped.review_write(
                loaded,
                &loaded.record,
                event(
                    if q.resolved { "resolve" } else { "unresolve" },
                    &actor,
                    serde_json::json!({ "comment": q.id }),
                ),
            );
            write.comments.push(Self::comment_row(&comment));
            saved = Some(comment);
            Ok(write)
        })?;
        saved.ok_or_else(|| Status::internal("comment was not saved"))
    }
    pub fn publish(
        &self,
        authorization: Option<&str>,
        q: p::PublishRequest,
    ) -> Result<p::PublishResponse, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        let actor = scoped.review_actor()?;
        text(&q.body, MAX_TEXT, "comment")?;
        let vote = q
            .vote
            .map(p::VoteValue::try_from)
            .transpose()
            .map_err(|_| Status::invalid_argument("unknown vote"))?;
        let mut published = Vec::new();
        let number = scoped.mutate_review(q.number, |loaded, comments| {
            if !loaded
                .record
                .revisions
                .iter()
                .any(|r| r.number == q.revision)
            {
                return Err(Status::invalid_argument("unknown revision"));
            }
            if vote.is_some() && loaded.state() != p::ReviewState::Open {
                return Err(Status::failed_precondition("only open reviews take votes"));
            }
            let now = now_seconds();
            let mut record = loaded.record.clone();
            published = comments
                .iter()
                .filter(|c| c.draft && c.author == actor && !c.deleted)
                .cloned()
                .map(|mut c| {
                    c.draft = false;
                    c.created_at = now;
                    c
                })
                .collect::<Vec<_>>();
            if !q.body.is_empty() {
                if comments.len() >= MAX_COMMENTS {
                    return Err(Status::resource_exhausted(
                        "a review has at most 10000 comments",
                    ));
                }
                published.push(p::ReviewComment {
                    id: comments.iter().map(|c| c.id).max().unwrap_or(0) + 1,
                    revision: q.revision,
                    author: actor.clone(),
                    body: q.body.clone(),
                    created_at: now,
                    edited_at: now,
                    ..Default::default()
                });
            }
            let ballot = vote.map(|value| p::ReviewVote {
                account: actor.clone(),
                revision: q.revision,
                value: value as i32,
                at: now,
            });
            if published.is_empty() && ballot.is_none() {
                return Err(Status::invalid_argument("nothing to publish"));
            }
            if let Some(ballot) = &ballot {
                record.votes.retain(|v| v.account != actor);
                record.votes.push(ballot.clone());
            }
            record.updated_at = now;
            let mut write = scoped.review_write(
                loaded,
                &record,
                event(
                    "publish",
                    &actor,
                    serde_json::json!({
                        "revision": q.revision,
                        "comments": published.iter().map(|c| c.id).collect::<Vec<_>>(),
                        "vote": vote.map(|v| v.as_str_name()),
                    }),
                ),
            );
            write.comments = published.iter().map(Self::comment_row).collect();
            write.votes = ballot
                .iter()
                .map(|b| ReviewVoteRow {
                    account: b.account.clone(),
                    data: b.encode_to_vec(),
                })
                .collect();
            Ok(write)
        })?;
        Ok(p::PublishResponse {
            review: Some(scoped.load_review(number)?.record),
            comments: published,
        })
    }

    /// Whether `account` currently counts as an approver of `target`.
    fn eligible(&self, rule: Option<&ReviewRule>, account: &str, target: &[u8]) -> bool {
        if let Some(globs) = rule.and_then(|r| r.approvers.as_ref())
            && !globs
                .iter()
                .any(|g| crate::write::glob(g.as_bytes(), account.as_bytes()))
        {
            return false;
        }
        if self.repository_accounts.is_empty() {
            return true;
        }
        let Some(index) = self
            .repository_accounts
            .iter()
            .position(|a| a.name == account)
        else {
            return false;
        };
        let entry = &self.repository_accounts[index];
        let Some(grant) = entry.grant_for(self.record(), self.legacy.as_ref()) else {
            return false;
        };
        if entry.grant(grant).read_directories.is_some() {
            return false;
        }
        if rule.is_some_and(|r| r.approvers.is_some()) {
            return true;
        }
        if entry.grant(grant).write_policy.is_none() {
            return false;
        }
        let mut writer = self.clone();
        writer.writer_grant = Some((index, grant));
        writer.allows_ref(target)
    }
    /// Every path the revision's stack changes, relative to its base.
    fn stack_paths(&self, revision: &p::ReviewRevision) -> Result<Vec<Vec<u8>>, Status> {
        let repo = self.context_requested().to_vec();
        let base = (!revision.base.is_empty())
            .then(|| self.id(&repo, &revision.base))
            .transpose()?;
        let head = self.id(
            &repo,
            revision
                .changesets
                .last()
                .ok_or_else(|| Status::internal("empty review stack"))?,
        )?;
        Ok(kasu_derived::diff(
            &self.ops().ctx.repo,
            self.tree_of(base)?,
            self.tree_of(Some(head))?,
        )
        .map_err(status)?
        .iter()
        .map(|change| change.path().as_bytes().to_vec())
        .collect())
    }
    /// The caller's write access to `target`, as Submit would check it.
    fn caller_writes(&self, target: &[u8]) -> bool {
        if self.principal.is_some_and(Credential::anonymous_delegation) {
            return false;
        }
        let mut writer = self.clone();
        if let Some(account) = self.principal.and_then(Credential::account) {
            let entry = &self.repository_accounts[account];
            match entry.grant_for(self.record(), self.legacy.as_ref()) {
                Some(grant) if entry.grant(grant).write_policy.is_some() => {
                    writer.writer_grant = Some((account, grant));
                }
                _ => return false,
            }
        }
        self.record().state.writable() && writer.allows_ref(target)
    }
    fn evaluate(&self, loaded: &Loaded, comments: &[p::ReviewComment]) -> p::SubmitStatus {
        let record = &loaded.record;
        let rule = self.review_rule(&record.target).cloned();
        let defaults = ReviewRule {
            required_approvals: 0,
            ..Default::default()
        };
        let effective = rule.as_ref().unwrap_or(&defaults);
        let latest = loaded.latest();
        let mut unmet = Vec::new();
        if loaded.state() != p::ReviewState::Open {
            unmet.push("the review is not open".to_owned());
        }
        let excluded = |account: &str| {
            !effective.allow_self_approval
                && (record.author == account
                    || record.revisions.iter().any(|r| r.created_by == account))
        };
        let unchanged_since = |revision: u32| {
            record
                .revisions
                .iter()
                .filter(|r| r.number >= revision)
                .all(|r| r.fingerprint == latest.fingerprint)
                && record.revisions.iter().any(|r| r.number == revision)
        };
        let mut approvals = 0u32;
        for vote in &record.votes {
            if !self.eligible(rule.as_ref(), &vote.account, &record.target) {
                continue;
            }
            match p::VoteValue::try_from(vote.value) {
                Ok(p::VoteValue::Approve)
                    if !excluded(&vote.account)
                        && (vote.revision == latest.number
                            || (effective.keep_approvals_on_trivial_rebase
                                && unchanged_since(vote.revision))) =>
                {
                    approvals += 1;
                }
                Ok(p::VoteValue::RequestChanges) if effective.block_on_request_changes => {
                    unmet.push(format!("{} requested changes", vote.account));
                }
                _ => {}
            }
        }
        if approvals < effective.required_approvals {
            unmet.push(format!(
                "{approvals} of {} required approvals",
                effective.required_approvals
            ));
        }
        if !effective.owners.is_empty() {
            match self.stack_paths(latest) {
                Ok(paths) => {
                    for owners in &effective.owners {
                        if !paths.iter().any(|p| owners.owns(p)) {
                            continue;
                        }
                        let count = record
                            .votes
                            .iter()
                            .filter(|vote| {
                                vote.value == p::VoteValue::Approve as i32
                                    && owners.approvers.iter().any(|g| {
                                        crate::write::glob(g.as_bytes(), vote.account.as_bytes())
                                    })
                                    && self.eligible(None, &vote.account, &record.target)
                                    && !excluded(&vote.account)
                                    && (vote.revision == latest.number
                                        || (effective.keep_approvals_on_trivial_rebase
                                            && unchanged_since(vote.revision)))
                            })
                            .count() as u32;
                        if count < owners.required_approvals {
                            unmet.push(format!(
                                "{count} of {} approvals from the owners of {}",
                                owners.required_approvals,
                                owners.paths.join(", ")
                            ));
                        }
                    }
                }
                Err(_) => unmet.push("the changed paths could not be computed".to_owned()),
            }
        }
        if effective.require_resolved_threads
            && comments
                .iter()
                .any(|c| c.parent.is_none() && !c.draft && !c.deleted && !c.resolved)
        {
            unmet.push("unresolved threads remain".to_owned());
        }
        if !self.caller_writes(&record.target) {
            unmet.push("you cannot write to the target ref".to_owned());
        }
        p::SubmitStatus {
            ready: unmet.is_empty(),
            unmet,
            approvals,
            required_approvals: effective.required_approvals,
            protected: rule.is_some(),
            methods: effective.methods.iter().map(|m| m.code() as i32).collect(),
            revision: latest.number,
        }
    }
    pub fn submit_status(
        &self,
        authorization: Option<&str>,
        q: p::GetReviewRequest,
    ) -> Result<p::SubmitStatus, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Read)?;
        let loaded = scoped.load_review(q.number)?;
        let comments = scoped.comments(q.number)?;
        Ok(scoped.evaluate(&loaded, &comments))
    }
    fn submitter_signature(&self, actor: &str) -> c::Signature {
        let account = self.principal_account();
        c::Signature {
            name: account
                .and_then(|a| a.display_name.clone())
                .unwrap_or_else(|| actor.to_owned())
                .into(),
            email: account
                .and_then(|a| a.email.clone())
                .unwrap_or_else(|| format!("{actor}@kasu.invalid"))
                .into(),
            timestamp: now_seconds(),
            tz: "+0000".into(),
        }
    }
    /// Put the stack on `onto`; Err carries the conflicting member's index.
    fn land_stack(
        &self,
        loaded: &Loaded,
        method: SubmitMethodName,
        onto: Option<ChangesetId>,
        actor: &str,
        trailer: bool,
    ) -> Result<Result<Vec<ChangesetId>, (u32, p::RewriteResponse)>, Status> {
        let repo = self.context_requested().to_vec();
        let latest = loaded.latest();
        let stack = latest
            .changesets
            .iter()
            .map(|id| self.id(&repo, id))
            .collect::<Result<Vec<_>, _>>()?;
        let base = (!latest.base.is_empty())
            .then(|| self.id(&repo, &latest.base))
            .transpose()?;
        let onto = onto.or(base);
        let ops = self.ops();
        let repository = &ops.ctx.repo;
        let committer = self.submitter_signature(actor);
        let suffix = if trailer {
            format!(
                "\n\nReview: {}#{}",
                String::from_utf8_lossy(&repo),
                loaded.record.number
            )
        } else {
            String::new()
        };
        let finish = |message: &[u8]| {
            let mut message = message.to_vec();
            while message.last() == Some(&b'\n') {
                message.pop();
            }
            message.extend_from_slice(suffix.as_bytes());
            message.push(b'\n');
            message
        };
        let create = |parents: Vec<ChangesetId>,
                      changes: Vec<c::FileChange>,
                      author: c::Signature,
                      message: Vec<u8>|
         -> Result<ChangesetId, Status> {
            let id = ops
                .create_changeset(&c::Changeset {
                    parents,
                    author,
                    committer: committer.clone(),
                    message: message.into(),
                    changes,
                    git_provenance: None,
                })
                .map_err(status)?;
            self.created(id)?;
            Ok(id)
        };
        match method {
            SubmitMethodName::Squash => {
                let tip = *stack.last().expect("a non-empty stack");
                let head = match onto {
                    Some(onto) if Some(onto) != base => {
                        let merged = kasu_ops::merge_trees(
                            repository,
                            self.tree_of(base)?,
                            self.tree_of(Some(onto))?,
                            self.tree_of(Some(tip))?,
                        )
                        .map_err(status)?;
                        match merged.tree {
                            Some(tree) => tree,
                            None => {
                                let conflict = kasu_ops::RewriteResult::Conflicted {
                                    stage: kasu_ops::ConflictStage::Result,
                                    bases: base.into_iter().collect(),
                                    conflicts: merged.conflicts,
                                };
                                return Ok(Err((0, self.rewritten(conflict)?)));
                            }
                        }
                    }
                    _ => self.tree_of(Some(tip))?,
                };
                let changes =
                    kasu_derived::diff(repository, self.tree_of(onto)?, head).map_err(status)?;
                let author = repository.changeset(stack[0]).map_err(status)?.author;
                let author = match self
                    .repository_accounts
                    .iter()
                    .find(|a| a.name == loaded.record.author)
                {
                    Some(account) if account.display_name.is_some() && account.email.is_some() => {
                        c::Signature {
                            name: account.display_name.clone().unwrap_or_default().into(),
                            email: account.email.clone().unwrap_or_default().into(),
                            ..committer.clone()
                        }
                    }
                    _ => author,
                };
                let mut message = loaded.record.title.clone();
                if !loaded.record.description.is_empty() {
                    message.extend_from_slice(b"\n\n");
                    message.extend_from_slice(&loaded.record.description);
                }
                Ok(Ok(vec![create(
                    onto.into_iter().collect(),
                    changes,
                    author,
                    finish(&message),
                )?]))
            }
            SubmitMethodName::Rebase => {
                let mut landed = Vec::with_capacity(stack.len());
                let mut parent = onto;
                for (index, id) in stack.iter().enumerate() {
                    let original = repository.changeset(*id).map_err(status)?;
                    let in_place = original.parents.first().copied() == parent;
                    if in_place && !trailer {
                        landed.push(*id);
                        parent = Some(*id);
                        continue;
                    }
                    let (parents, source) = if in_place {
                        (original.parents.clone(), *id)
                    } else {
                        let onto = parent.expect("a rebase has a parent");
                        match ops.rebase(*id, onto, committer.clone()).map_err(status)? {
                            kasu_ops::RewriteResult::Created { id: rebased, .. } => {
                                if !trailer {
                                    self.created(rebased)?;
                                    landed.push(rebased);
                                    parent = Some(rebased);
                                    continue;
                                }
                                (vec![onto], rebased)
                            }
                            conflict => {
                                return Ok(Err((index as u32, self.rewritten(conflict)?)));
                            }
                        }
                    };
                    let changes = kasu_derived::diff(
                        repository,
                        self.tree_of(parents.first().copied())?,
                        self.tree_of(Some(source))?,
                    )
                    .map_err(status)?;
                    let id = create(parents, changes, original.author, finish(&original.message))?;
                    landed.push(id);
                    parent = Some(id);
                }
                Ok(Ok(landed))
            }
        }
    }
    pub fn submit(
        &self,
        authorization: Option<&str>,
        q: p::SubmitRequest,
    ) -> Result<p::SubmitResponse, Status> {
        let scoped = self.review_scope(authorization, &q.repo, Mode::Write)?;
        scoped.submit_scoped(q)
    }
    fn submit_scoped(&self, q: p::SubmitRequest) -> Result<p::SubmitResponse, Status> {
        let actor = self.review_actor()?;
        let method = match p::SubmitMethod::try_from(q.method) {
            Ok(p::SubmitMethod::Rebase) => SubmitMethodName::Rebase,
            Ok(p::SubmitMethod::Squash) => SubmitMethodName::Squash,
            Err(_) => return Err(Status::invalid_argument("unknown submit method")),
        };
        if q.idempotency_key.is_empty() || q.idempotency_key.len() > 256 {
            return Err(Status::invalid_argument(
                "Submit requires a 1..256-byte key",
            ));
        }
        let attempts = match q.max_attempts {
            0 => 8,
            n @ 1..=32 => n,
            _ => {
                return Err(Status::invalid_argument(
                    "Submit permits at most 32 attempts",
                ));
            }
        };
        let timeout = match q.timeout_ms {
            0 => 5000,
            n @ 1..=30_000 => n,
            _ => return Err(Status::invalid_argument("Submit permits at most 30000 ms")),
        };
        let deadline = Instant::now() + Duration::from_millis(timeout.into());
        let mut fingerprint = Encoder::default();
        fingerprint.raw(b"kasu.review.submit.v1");
        fingerprint.u64(q.number);
        fingerprint.u64(q.expected_revision.into());
        fingerprint.u64(q.method as u64);
        let fingerprint = ObjectId::hash(&fingerprint.0).0.to_vec();
        let key = [b"submit\0".as_slice(), &q.idempotency_key].concat();
        let store = self.review_store()?.clone();
        let replay = |response: Vec<u8>| -> Result<p::SubmitResponse, Status> {
            let mut response: p::SubmitResponse = decode(&response)?;
            response.replayed = true;
            response.review = Some(self.load_review(q.number)?.record);
            Ok(response)
        };
        let mut actual = None;
        for attempt in 1..=attempts {
            if Instant::now() >= deadline {
                break;
            }
            if let Some(record) = store
                .review_request(self.key(), &actor, &key)
                .map_err(status)?
            {
                if record.fingerprint != fingerprint {
                    return Err(Status::invalid_argument(
                        "Submit key was used for a different request",
                    ));
                }
                return replay(record.response);
            }
            let loaded = self.load_review(q.number)?;
            let comments = self.comments(q.number)?;
            let not_ready = |status: p::SubmitStatus, message: &str| p::SubmitResponse {
                outcome: p::SubmitOutcome::NotReady as i32,
                status: Some(status),
                message: message.into(),
                review: Some(loaded.record.clone()),
                ..Default::default()
            };
            let status_now = self.evaluate(&loaded, &comments);
            if loaded.latest().number != q.expected_revision {
                return Ok(not_ready(status_now, "the review has a newer revision"));
            }
            if !status_now.ready {
                return Ok(not_ready(status_now, "the review is not ready to submit"));
            }
            let rule = self.review_rule(&loaded.record.target).cloned();
            let denied = |message: &str| p::SubmitResponse {
                outcome: p::SubmitOutcome::Denied as i32,
                message: message.into(),
                review: Some(loaded.record.clone()),
                ..Default::default()
            };
            if rule.as_ref().is_some_and(|r| !r.methods.contains(&method)) {
                return Ok(denied("this submit method is not allowed for the target"));
            }
            let target = loaded.record.target.clone();
            let expected = self.refs.resolve(self.key(), &target).map_err(status)?;
            if !self.allows_ref(&target) {
                return Ok(denied("ref is not writable"));
            }
            if expected.target.is_none() && !self.allows_create() {
                return Ok(denied("ref creation is disabled"));
            }
            let trailer = rule.as_ref().is_none_or(|r| r.trailer);
            let landed = match self.land_stack(&loaded, method, expected.target, &actor, trailer)? {
                Ok(landed) => landed,
                Err((index, conflict)) => {
                    return Ok(p::SubmitResponse {
                        outcome: p::SubmitOutcome::Conflict as i32,
                        conflict: Some(conflict),
                        conflict_index: index,
                        actual: Some(reference(&target, expected)),
                        review: Some(loaded.record.clone()),
                        message: "the stack conflicts with the target".into(),
                        ..Default::default()
                    });
                }
            };
            let tip = *landed.last().expect("a landed stack is not empty");
            if let Some(onto) = expected.target
                && !self.ops().is_ancestor(onto, tip).map_err(status)?
            {
                return Ok(denied("Submit must fast-forward the target"));
            }
            if let Err(error) = self.check_changeset_acl(expected.target, Some(tip)) {
                if error.code() == tonic::Code::PermissionDenied {
                    return Ok(denied(error.message()));
                }
                return Err(error);
            }
            let ops = self.ops();
            ops.engine
                .derive::<kasu_git::GitCommit>(&ops.ctx, tip, &self.full_projection())
                .map_err(status)?;
            let new_ref = RefState {
                target: Some(tip),
                version: expected.version + 1,
            };
            let now = now_seconds();
            let mut record = loaded.record.clone();
            record.state = p::ReviewState::Submitted as i32;
            record.updated_at = now;
            record.submitted = Some(p::ReviewSubmission {
                by: actor.clone(),
                at: now,
                method: method.code() as i32,
                landed: landed.iter().map(|id| id.0.to_vec()).collect(),
                ref_version: new_ref.version,
            });
            let mut response = p::SubmitResponse {
                outcome: p::SubmitOutcome::Landed as i32,
                landed: landed.iter().map(|id| id.0.to_vec()).collect(),
                actual: Some(reference(&target, new_ref)),
                status: Some(status_now),
                message: String::new(),
                ..Default::default()
            };
            let mut write = self.review_write(
                &loaded,
                &record,
                event(
                    "submit",
                    &actor,
                    serde_json::json!({ "revision": q.expected_revision, "method": method }),
                ),
            );
            write.request = Some(ReviewRequestRecord {
                account: actor.clone(),
                key: key.clone(),
                fingerprint: fingerprint.clone(),
                response: response.encode_to_vec(),
            });
            write.ref_update = Some(ReviewRefUpdate {
                update: RefUpdate {
                    name: target.clone(),
                    expected,
                    new: Some(tip),
                },
                actor: self.write_actor(b"review"),
            });
            match store.write_review(self.key(), &write).map_err(status)? {
                ReviewWriteResult::Applied { number } => {
                    response.review = Some(self.load_review(number)?.record);
                    return Ok(response);
                }
                ReviewWriteResult::Replayed { response } => return replay(response),
                ReviewWriteResult::RefMismatch(current) => {
                    actual = Some(reference(&target, current));
                }
                ReviewWriteResult::VersionMismatch => {}
                other => return Err(conflict_status(other)),
            }
            let pause = Duration::from_millis((2u64 << attempt.min(6)).min(200));
            std::thread::sleep(pause.min(deadline.saturating_duration_since(Instant::now())));
        }
        Ok(p::SubmitResponse {
            outcome: p::SubmitOutcome::Retry as i32,
            actual,
            message: "attempt budget or deadline exhausted".into(),
            ..Default::default()
        })
    }
}
fn reference(name: &[u8], state: RefState) -> p::Ref {
    p::Ref {
        name: name.to_vec(),
        target: state.target.map(|id| id.0.to_vec()),
        version: state.version,
    }
}

#[derive(Clone)]
pub struct ReviewService {
    state: Arc<Backend>,
}
impl ReviewService {
    pub fn new(state: Arc<Backend>) -> Self {
        Self { state }
    }
    pub fn into_server(self) -> crate::ObservedService<ReviewServer<Self>> {
        use tower::Layer;
        let layer = crate::ServerLayer::grpc(self.state.clone());
        layer.layer(
            ReviewServer::new(self)
                .max_decoding_message_size(4 * 1024 * 1024)
                .max_encoding_message_size(64 * 1024 * 1024),
        )
    }
}
macro_rules! review {
    ($(($method:ident,$backend:ident,$request:ident,$response:ident,$path:literal)),* $(,)?) => {
        #[tonic::async_trait]
        impl Review for ReviewService {
            $(async fn $method(&self, request: Request<p::$request>) -> Result<Response<p::$response>, Status> {
                let authorization = request.metadata().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
                let input = request.into_inner();
                let state = self.state.clone();
                let result = crate::admission::blocking(move || state.$backend(authorization.as_deref(), input)).await.map_err(|_| Status::internal("review worker failed"))??;
                Ok(Response::new(result))
            })*
        }
        $(async fn $method(State(state): State<Arc<Backend>>, headers: HeaderMap, Json(input): Json<p::$request>) -> Result<Json<p::$response>, crate::transport::HttpError> {
            let authorization = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let result = crate::admission::blocking(move || state.$backend(authorization.as_deref(), input)).await.map_err(|_| crate::transport::HttpError(Status::internal("review worker failed")))?.map_err(crate::transport::HttpError)?;
            Ok(Json(result))
        })*
        pub fn review_http_router(state: Arc<Backend>) -> Router {
            Router::new()$(.route($path, post($method)))*.layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)).with_state(state)
        }
    };
}
review! {
    (create_review, create_review, CreateReviewRequest, ReviewRecord, "/v1/review/create"),
    (get_review, get_review, GetReviewRequest, ReviewRecord, "/v1/review/get"),
    (list_reviews, list_reviews, ListReviewsRequest, ListReviewsResponse, "/v1/review/list"),
    (update_review, update_review, UpdateReviewRequest, ReviewRecord, "/v1/review/update"),
    (add_revision, add_revision, AddRevisionRequest, ReviewRecord, "/v1/review/add-revision"),
    (rebase_review, rebase_review, RebaseReviewRequest, RebaseReviewResponse, "/v1/review/rebase"),
    (list_comments, list_comments, ListCommentsRequest, ListCommentsResponse, "/v1/review/list-comments"),
    (publish, publish, PublishRequest, PublishResponse, "/v1/review/publish"),
    (save_draft, save_draft, SaveDraftRequest, ReviewComment, "/v1/review/save-draft"),
    (edit_comment, edit_comment, EditCommentRequest, ReviewComment, "/v1/review/edit-comment"),
    (resolve_thread, resolve_thread, ResolveThreadRequest, ReviewComment, "/v1/review/resolve-thread"),
    (get_submit_status, submit_status, GetReviewRequest, SubmitStatus, "/v1/review/submit-status"),
    (submit, submit, SubmitRequest, SubmitResponse, "/v1/review/submit"),
}
