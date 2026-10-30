use crate::{RefState, RefUpdate};
use kasu_core::Result;

pub const REVIEW_OPEN: u8 = 0;
pub const REVIEW_SUBMITTED: u8 = 1;
pub const REVIEW_CLOSED: u8 = 2;

/// A review's mutable header. The indexed columns are separate from `data`,
/// whose encoding belongs to the server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRow {
    /// Unique within the repository and never reused; 0 allocates on create.
    pub number: u64,
    pub version: u64,
    pub state: u8,
    pub target: Vec<u8>,
    /// The tip of the latest revision. Open reviews are unique per
    /// (target, head).
    pub head: Vec<u8>,
    pub author: String,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRevisionRow {
    pub revision: u32,
    pub data: Vec<u8>,
}
/// The latest vote of one account.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewVoteRow {
    pub account: String,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewCommentRow {
    pub id: u64,
    pub author: String,
    pub draft: bool,
    pub data: Vec<u8>,
}
/// The result of an idempotent review request, keyed per account.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewRequestRecord {
    pub account: String,
    pub key: Vec<u8>,
    pub fingerprint: Vec<u8>,
    /// Empty records the review number the write allocated or updated, as
    /// eight big-endian bytes.
    pub response: Vec<u8>,
}
/// A ref change committed with the review write, as Submit does.
#[derive(Clone, Debug)]
pub struct ReviewRefUpdate {
    pub update: RefUpdate,
    /// See `delegated_actor` for the encoding.
    pub actor: Vec<u8>,
}
#[derive(Clone, Debug, Default)]
pub struct ReviewFilter {
    pub state: Option<u8>,
    pub target: Option<Vec<u8>>,
    pub author: Option<String>,
}
/// One atomic review change: the header CAS, child rows, an optional
/// idempotency record and an optional ref CAS with its ref_log entry.
#[derive(Clone, Debug)]
pub struct ReviewWrite {
    pub review: ReviewRow,
    /// The stored version this write replaces; 0 creates the review.
    pub expected_version: u64,
    pub revision: Option<ReviewRevisionRow>,
    /// Upserted by account.
    pub votes: Vec<ReviewVoteRow>,
    /// Upserted by id.
    pub comments: Vec<ReviewCommentRow>,
    /// Appended to review_log unless empty.
    pub event: Vec<u8>,
    pub request: Option<ReviewRequestRecord>,
    pub ref_update: Option<ReviewRefUpdate>,
    pub time: i64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewWriteResult {
    Applied {
        number: u64,
    },
    /// The request key already has a result.
    Replayed {
        response: Vec<u8>,
    },
    VersionMismatch,
    /// Another open review has the same target and head.
    Duplicate(u64),
    RefMismatch(RefState),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewEvent {
    pub seq: u64,
    pub number: u64,
    pub time: i64,
    pub data: Vec<u8>,
}
/// Review records, partitioned by repository key. Writes are refused while
/// the repository is not writable.
pub trait ReviewStore: Send + Sync {
    fn review(&self, repo: &[u8], number: u64) -> Result<Option<ReviewRow>>;
    /// Newest first, numbers below `before` (0 for no bound).
    fn list_reviews(
        &self,
        repo: &[u8],
        filter: &ReviewFilter,
        before: u64,
        limit: usize,
    ) -> Result<Vec<ReviewRow>>;
    fn review_revisions(&self, repo: &[u8], number: u64) -> Result<Vec<ReviewRevisionRow>>;
    fn review_votes(&self, repo: &[u8], number: u64) -> Result<Vec<ReviewVoteRow>>;
    fn review_comments(&self, repo: &[u8], number: u64) -> Result<Vec<ReviewCommentRow>>;
    fn review_request(
        &self,
        repo: &[u8],
        account: &str,
        key: &[u8],
    ) -> Result<Option<ReviewRequestRecord>>;
    fn write_review(&self, repo: &[u8], write: &ReviewWrite) -> Result<ReviewWriteResult>;
    fn review_log(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<ReviewEvent>>;
    /// Remove up to `limit` review rows of a deleted repository.
    fn purge_reviews(&self, repo: &[u8], limit: usize) -> Result<usize>;
}
pub(crate) fn validate_write(write: &ReviewWrite) -> Result<()> {
    let review = &write.review;
    if review.state > REVIEW_CLOSED
        || review.version != write.expected_version + 1
        || (write.expected_version == 0) != (review.number == 0)
        || review.target.is_empty()
        || review.author.is_empty()
        || write
            .request
            .as_ref()
            .is_some_and(|r| r.key.is_empty() || r.key.len() > 256)
        || review.version > i64::MAX as u64
    {
        return Err(kasu_core::invalid("invalid review write"));
    }
    Ok(())
}
pub(crate) fn allocated(number: u64) -> Vec<u8> {
    number.to_be_bytes().to_vec()
}
