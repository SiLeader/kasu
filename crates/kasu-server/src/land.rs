use crate::{Backend, backend::status};
use kasu_core::{Encoder, ObjectId};
use kasu_ops::RewriteResult;
use kasu_proto::v1 as p;
use kasu_store::{LandCommit, LandCommitResult, LandRecord, RefState};
use prost::Message;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tonic::Status;

#[derive(Clone)]
pub struct LandCancellation {
    sender: Arc<tokio::sync::watch::Sender<bool>>,
}
impl Default for LandCancellation {
    fn default() -> Self {
        Self::new()
    }
}
impl LandCancellation {
    pub fn new() -> Self {
        Self {
            sender: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }
    async fn cancelled(&self) {
        let mut receiver = self.sender.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}
struct CancelOnDrop {
    cancellation: LandCancellation,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

#[derive(Clone, Debug)]
pub struct LandOperation {
    request: p::LandRequest,
    fingerprint: ObjectId,
    storage_key: Vec<u8>,
    attempts: u32,
    timeout: Duration,
}
impl LandOperation {
    pub fn max_attempts(&self) -> u32 {
        self.attempts
    }
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
    /// Bounded exponential backoff with request/attempt-specific deterministic jitter.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let cap = (2u64 << attempt.min(7)).min(200);
        let mut input = self.fingerprint.0.to_vec();
        input.extend(attempt.to_be_bytes());
        let jitter = u64::from_be_bytes(ObjectId::hash(&input).0[..8].try_into().unwrap());
        Duration::from_millis(cap / 2 + jitter % (cap / 2 + 1))
    }
    pub fn retry(&self, attempts: u32, actual: Option<p::Ref>, message: &str) -> p::LandResponse {
        p::LandResponse {
            outcome: p::LandOutcome::Retry as i32,
            attempts,
            actual,
            message: message.into(),
            ..Default::default()
        }
    }
}
pub enum LandPreparation {
    Complete(p::LandResponse),
    Ready(PreparedLand),
}
/// Opaque prepared attempt, usable by the deterministic scheduler in kasu-sim.
pub struct PreparedLand {
    operation: LandOperation,
    commit: LandCommit,
    attempt: u32,
}
pub enum LandAttempt {
    Complete(p::LandResponse),
    Contended(p::Ref),
    Cancelled,
}

fn reference(name: &[u8], state: RefState) -> p::Ref {
    p::Ref {
        name: name.to_vec(),
        target: state.target.map(|id| id.0.to_vec()),
        version: state.version,
    }
}
fn receipt(
    record: LandRecord,
    fingerprint: ObjectId,
    replayed: bool,
) -> Result<p::LandResponse, Status> {
    if record.fingerprint != fingerprint {
        return Err(Status::invalid_argument(
            "Land key was used for a different request",
        ));
    }
    let mut response = p::LandResponse::decode(record.response.as_slice())
        .map_err(|_| Status::internal("corrupt Land result"))?;
    response.replayed = replayed;
    Ok(response)
}
impl Backend {
    pub fn begin_land(&self, request: p::LandRequest) -> Result<LandOperation, Status> {
        self.repo(&request.repo)?;
        kasu_git::validate_branch_name(&request.name).map_err(status)?;
        if request.id.len() != 32
            || request.idempotency_key.is_empty()
            || request.idempotency_key.len() > 256
        {
            return Err(Status::invalid_argument(
                "Land requires a 32-byte changeset ID and a 1..256-byte key",
            ));
        }
        let attempts = if request.max_attempts == 0 {
            8
        } else {
            request.max_attempts
        };
        let timeout = if request.timeout_ms == 0 {
            5000
        } else {
            request.timeout_ms
        };
        if attempts > 32 || timeout > 30_000 {
            return Err(Status::invalid_argument(
                "Land permits at most 32 attempts and 30000 ms",
            ));
        }
        let mut encoded = Encoder::default();
        encoded.raw(b"kasu.land\0v1");
        for bytes in [&request.repo, &request.name, &request.id] {
            encoded.bytes(bytes);
        }
        encoded.bytes(b"api");
        if !request.required_ancestor.is_empty() {
            self.id(&request.repo, &request.required_ancestor)?;
            // Leave the fingerprint of legacy unguarded requests unchanged.
            encoded.bytes(b"required-ancestor-v1");
            encoded.bytes(&request.required_ancestor);
        }
        let mut storage_key = request.idempotency_key.clone();
        if let Some(principal) = self.write_principal() {
            encoded.bytes(b"repository-account-v1");
            encoded.bytes(principal.as_bytes());
            let mut key = Encoder::default();
            key.bytes(principal.as_bytes());
            key.bytes(&storage_key);
            if let Some(scope) = &self.read_scope {
                encoded.bytes(&scope.namespace);
                key.bytes(&scope.namespace);
            }
            storage_key = b"kasu.account.land.v1\0".to_vec();
            storage_key.extend(ObjectId::hash(&key.0).0);
        }
        Ok(LandOperation {
            request,
            storage_key,
            fingerprint: ObjectId::hash(&encoded.0),
            attempts,
            timeout: Duration::from_millis(timeout.into()),
        })
    }
    fn land_policy(&self, name: &[u8], expected: RefState) -> Option<&'static str> {
        if !self.allows_ref(name) {
            Some("ref is not writable")
        } else if expected.target.is_none() && !self.allows_create() {
            Some("ref creation is disabled")
        } else {
            None
        }
    }
    fn land_existing(&self, operation: &LandOperation) -> Result<Option<p::LandResponse>, Status> {
        self.refs
            .land_record(&operation.request.repo, &operation.storage_key)
            .map_err(status)?
            .map(|record| receipt(record, operation.fingerprint, true))
            .transpose()
    }
    pub fn prepare_land(
        &self,
        operation: &LandOperation,
        attempt: u32,
    ) -> Result<LandPreparation, Status> {
        let q = &operation.request;
        let source = self.id(&q.repo, &q.id)?;
        if attempt == 0 || attempt > operation.attempts {
            return Err(Status::invalid_argument(
                "attempt is outside the Land budget",
            ));
        }
        if let Some(response) = self.land_existing(operation)? {
            return Ok(LandPreparation::Complete(response));
        }
        let expected = self.refs.resolve(&q.repo, &q.name).map_err(status)?;
        if !q.required_ancestor.is_empty() {
            let ancestor = self.id(&q.repo, &q.required_ancestor)?;
            let allowed = match expected.target {
                Some(head) => self.ops().is_ancestor(ancestor, head).map_err(status)?,
                None => false,
            };
            if !allowed {
                return Ok(LandPreparation::Complete(p::LandResponse {
                    outcome: p::LandOutcome::Denied as i32,
                    actual: Some(reference(&q.name, expected)),
                    attempts: attempt,
                    message: "required ancestor is not on the current branch".into(),
                    ..Default::default()
                }));
            }
        }
        if let Some(message) = self.land_policy(&q.name, expected) {
            return Ok(LandPreparation::Complete(p::LandResponse {
                outcome: p::LandOutcome::Denied as i32,
                attempts: attempt,
                actual: Some(reference(&q.name, expected)),
                message: message.into(),
                ..Default::default()
            }));
        }
        let mut response = p::LandResponse {
            outcome: p::LandOutcome::Landed as i32,
            attempts: attempt,
            ..Default::default()
        };
        let mut new = None;
        let candidate = match expected.target {
            None => Some(source),
            Some(onto) => {
                self.ops()
                    .ctx
                    .repo
                    .require_membership(&q.repo, onto)
                    .map_err(status)?;
                if self.ops().is_ancestor(source, onto).map_err(status)? {
                    Some(onto)
                } else {
                    let cs = self.ops().ctx.repo.changeset(source).map_err(status)?;
                    if cs.parents.first() == Some(&onto) {
                        Some(source)
                    } else {
                        match self
                            .ops()
                            .rebase(source, onto, cs.committer)
                            .map_err(status)?
                        {
                            RewriteResult::Created { id, .. } => Some(id),
                            conflict @ RewriteResult::Conflicted { .. } => {
                                response.outcome = p::LandOutcome::Conflict as i32;
                                response.conflict = Some(self.rewritten(conflict)?);
                                None
                            }
                        }
                    }
                }
            }
        };
        if let Some(mut candidate) = candidate {
            if let Some(onto) = expected.target {
                if self.ops().root_tree(candidate).map_err(status)?
                    == self.ops().root_tree(onto).map_err(status)?
                {
                    candidate = onto;
                }
                if !self.ops().is_ancestor(onto, candidate).map_err(status)? {
                    return Err(Status::permission_denied("Land must fast-forward the ref"));
                }
            }
            self.created(candidate)?;
            if let Err(error) = self.check_changeset_acl(expected.target, Some(candidate)) {
                if error.code() == tonic::Code::PermissionDenied {
                    return Ok(LandPreparation::Complete(p::LandResponse {
                        outcome: p::LandOutcome::Denied as i32,
                        attempts: attempt,
                        message: error.message().into(),
                        ..Default::default()
                    }));
                }
                return Err(error);
            }
            let ops = self.ops();
            ops.engine
                .derive::<kasu_git::GitCommit>(
                    &ops.ctx,
                    candidate,
                    &kasu_git::ProjectionId::full(q.repo.clone()),
                )
                .map_err(status)?;
            response.id = Some(candidate.0.to_vec());
            if expected.target != Some(candidate) {
                new = Some(candidate);
            }
        }
        let actual = RefState {
            target: new.or(expected.target),
            version: expected
                .version
                .checked_add(u64::from(new.is_some()))
                .ok_or_else(|| Status::invalid_argument("ref version exhausted"))?,
        };
        response.actual = Some(reference(&q.name, actual));
        Ok(LandPreparation::Ready(PreparedLand {
            attempt,
            operation: operation.clone(),
            commit: LandCommit {
                name: q.name.clone(),
                expected,
                new,
                record: LandRecord {
                    fingerprint: operation.fingerprint,
                    response: response.encode_to_vec(),
                },
            },
        }))
    }
    pub fn commit_land(
        &self,
        prepared: PreparedLand,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<LandAttempt, Status> {
        let operation = &prepared.operation;
        let q = &operation.request;
        self.id(&q.repo, &q.id)?;
        if let Some(response) = self.land_existing(operation)? {
            return Ok(LandAttempt::Complete(response));
        }
        if let Some(message) = self.land_policy(&q.name, prepared.commit.expected) {
            return Ok(LandAttempt::Complete(p::LandResponse {
                outcome: p::LandOutcome::Denied as i32,
                attempts: prepared.attempt,
                message: message.into(),
                ..Default::default()
            }));
        }
        if let Some(new) = prepared.commit.new
            && let Err(error) = self.check_changeset_acl(prepared.commit.expected.target, Some(new))
        {
            if error.code() == tonic::Code::PermissionDenied {
                return Ok(LandAttempt::Complete(p::LandResponse {
                    outcome: p::LandOutcome::Denied as i32,
                    attempts: prepared.attempt,
                    message: error.message().into(),
                    ..Default::default()
                }));
            }
            return Err(error);
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Status::internal("clock before epoch"))?
            .as_secs() as i64;
        Ok(
            match self
                .refs
                .commit_land(
                    &q.repo,
                    &operation.storage_key,
                    &prepared.commit,
                    self.write_actor(b"api"),
                    time,
                    cancelled,
                )
                .map_err(status)?
            {
                LandCommitResult::Recorded { record, replayed } => {
                    LandAttempt::Complete(receipt(record, operation.fingerprint, replayed)?)
                }
                LandCommitResult::Mismatch(actual) => {
                    LandAttempt::Contended(reference(&q.name, actual))
                }
                LandCommitResult::Cancelled => LandAttempt::Cancelled,
            },
        )
    }

    pub async fn land(self: Arc<Self>, request: p::LandRequest) -> Result<p::LandResponse, Status> {
        self.land_with_cancellation(request, LandCancellation::new())
            .await
    }
    pub async fn land_with_cancellation(
        self: Arc<Self>,
        request: p::LandRequest,
        cancellation: LandCancellation,
    ) -> Result<p::LandResponse, Status> {
        let operation = self.begin_land(request)?;
        let mut guard = CancelOnDrop {
            cancellation: cancellation.clone(),
            armed: true,
        };
        let deadline = Instant::now() + operation.timeout;
        let mut actual = None;
        for attempt in 1..=operation.attempts {
            if cancellation.is_cancelled() || Instant::now() >= deadline {
                return Ok(operation.retry(attempt - 1, actual, "cancelled or deadline exceeded"));
            }
            let state = self.clone();
            let op = operation.clone();
            let cancel = cancellation.clone();
            let worker =
                crate::admission::blocking(move || match state.prepare_land(&op, attempt)? {
                    LandPreparation::Complete(response) => Ok(LandAttempt::Complete(response)),
                    LandPreparation::Ready(prepared) => state.commit_land(prepared, &|| {
                        cancel.is_cancelled() || Instant::now() >= deadline
                    }),
                });
            let outcome = tokio::select! {
                result = worker => result.map_err(|_| Status::internal("Land worker failed"))?,
                _ = cancellation.cancelled() => return Ok(operation.retry(attempt, actual, "cancelled")),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Ok(operation.retry(attempt, actual, "deadline exceeded")),
            };
            match outcome {
                Ok(LandAttempt::Complete(response)) => {
                    guard.armed = false;
                    return Ok(response);
                }
                Ok(LandAttempt::Contended(current)) => actual = Some(current),
                Ok(LandAttempt::Cancelled) => {
                    return Ok(operation.retry(attempt, actual, "cancelled or deadline exceeded"));
                }
                Err(error)
                    if matches!(
                        error.code(),
                        tonic::Code::Internal
                            | tonic::Code::Unavailable
                            | tonic::Code::DeadlineExceeded
                    ) =>
                {
                    tracing::warn!(attempt, error=%error, "Land attempt failed; retrying within budget");
                }
                Err(error) => return Err(error),
            }
            if attempt < operation.attempts {
                tokio::select! {
                    _ = tokio::time::sleep(operation.backoff(attempt)) => {},
                    _ = cancellation.cancelled() => return Ok(operation.retry(attempt, actual, "cancelled")),
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Ok(operation.retry(attempt, actual, "deadline exceeded")),
                }
            }
        }
        Ok(operation.retry(operation.attempts, actual, "attempt budget exhausted"))
    }
}
