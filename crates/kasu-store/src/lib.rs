#[cfg(any(feature = "postgres", feature = "s3"))]
mod blocking;
mod configured;
mod fs;
pub use configured::{DatabaseStore, Storage};
#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "postgres")]
pub use postgres::{PostgresOptions, PostgresStore};
#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "s3")]
pub use s3::{S3BlobStore, S3Options};
mod registry;
mod sqlite;
pub use registry::*;
mod tree_membership;
mod webhook;
pub use webhook::*;
mod review;
pub use review::*;

pub use fs::FsBlobStore;
use kasu_core::{
    BlobId, BlobMetadata, Canonical, Changeset, ChangesetId, Error, GitOid, ObjectId, Result, Tree,
    TreeId, invalid,
};
pub use sqlite::SqliteStore;

pub trait MetaStore: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn put_if_absent(&self, key: &[u8], value: &[u8]) -> Result<()>;
    /// Remove up to `limit` keys that start with `prefix` and return the count.
    /// Only indexes of deleted repositories are discarded this way.
    fn delete_prefix(&self, _prefix: &[u8], _limit: usize) -> Result<usize> {
        Err(invalid("backend does not support prefix deletion"))
    }
    /// Up to `limit` keys that start with `prefix`, in key order.
    fn scan_prefix(&self, _prefix: &[u8], _limit: usize) -> Result<Vec<Vec<u8>>> {
        Err(invalid("backend does not support prefix scans"))
    }
}
/// The exclusive upper bound of keys with `prefix`, or None when unbounded.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != u8::MAX {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}
pub trait BlobStore: Send + Sync {
    // Bytes on disk and wire are identical zstd frames.
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>>;
    fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()>;
    fn put(&self, raw: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::hash(raw);
        self.put_compressed(id, &compress(raw)?)?;
        Ok(id)
    }
    fn get(&self, id: ObjectId) -> Result<Vec<u8>> {
        let raw = decompress(&self.get_compressed(id)?)?;
        if ObjectId::hash(&raw) != id {
            return Err(invalid("corrupt object hash"));
        }
        Ok(raw)
    }
}
pub const MAX_OBJECT_SIZE: u64 = 256 * 1024 * 1024;
pub const MAX_COMPRESSED_OBJECT_SIZE: usize = MAX_OBJECT_SIZE as usize + 1024 * 1024;
pub fn compress(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() as u64 > MAX_OBJECT_SIZE {
        return Err(invalid("object too large"));
    }
    zstd::stream::encode_all(raw, 3).map_err(storage_error)
}
pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    if bytes.len() > MAX_COMPRESSED_OBJECT_SIZE {
        return Err(invalid("compressed object too large"));
    }
    let decoder = zstd::stream::read::Decoder::new(bytes).map_err(storage_error)?;
    let mut out = Vec::new();
    decoder
        .take(MAX_OBJECT_SIZE + 1)
        .read_to_end(&mut out)
        .map_err(storage_error)?;
    if out.len() as u64 > MAX_OBJECT_SIZE {
        return Err(invalid("decompressed object too large"));
    }
    Ok(out)
}
pub fn storage_error(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefState {
    pub target: Option<ChangesetId>,
    pub version: u64,
}
#[derive(Clone, Debug)]
pub struct RefUpdate {
    pub name: Vec<u8>,
    pub expected: RefState,
    pub new: Option<ChangesetId>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefLog {
    pub seq: u64,
    pub name: Vec<u8>,
    pub old: Option<ChangesetId>,
    pub new: Option<ChangesetId>,
    pub actor: Vec<u8>,
    /// The delegate (and its user) that acted for `actor`; empty otherwise.
    pub via: Vec<u8>,
    pub time: i64,
}
/// The `actor` argument of ref writes may carry the delegate that acted for
/// the account: `actor NUL via`. Stores keep the parts in separate columns.
pub fn delegated_actor(actor: &[u8], via: &[u8]) -> Vec<u8> {
    let mut value = actor.to_vec();
    value.push(0);
    value.extend(via);
    value
}
pub(crate) fn actor_parts(actor: &[u8]) -> (&[u8], Option<&[u8]>) {
    match actor.iter().position(|b| *b == 0) {
        Some(at) => (&actor[..at], Some(&actor[at + 1..])),
        None => (actor, None),
    }
}
#[derive(Clone, Copy, Debug)]
pub struct RefCapabilities {
    pub atomic_batch: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CasResult {
    Applied(Vec<RefState>),
    Mismatch { name: Vec<u8>, actual: RefState },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LandRecord {
    pub fingerprint: ObjectId,
    pub response: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct LandCommit {
    pub name: Vec<u8>,
    pub expected: RefState,
    /// None records a checked, terminal result without changing the ref.
    pub new: Option<ChangesetId>,
    pub record: LandRecord,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LandCommitResult {
    Recorded { record: LandRecord, replayed: bool },
    Mismatch(RefState),
    Cancelled,
}
pub trait RefStore: Send + Sync {
    fn land_record(&self, _repo: &[u8], _key: &[u8]) -> Result<Option<LandRecord>> {
        Err(invalid("backend does not support durable Land"))
    }
    /// The result, optional ref CAS, and ref_log append share one transaction.
    fn commit_land(
        &self,
        _repo: &[u8],
        _key: &[u8],
        _commit: &LandCommit,
        _actor: &[u8],
        _time: i64,
        _cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<LandCommitResult> {
        Err(invalid("backend does not support durable Land"))
    }
    fn capabilities(&self) -> RefCapabilities;
    fn resolve(&self, repo: &[u8], name: &[u8]) -> Result<RefState>;
    fn list(&self, repo: &[u8], prefix: &[u8]) -> Result<Vec<(Vec<u8>, RefState)>>;
    fn compare_and_swap(
        &self,
        repo: &[u8],
        updates: &[RefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<CasResult>;
    fn tail(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<RefLog>>;
}

pub struct Repository<'a> {
    pub meta: &'a dyn MetaStore,
    pub blobs: &'a dyn BlobStore,
}
impl Repository<'_> {
    fn blob_membership_key(repo: &[u8], id: BlobId) -> Vec<u8> {
        let mut encoded = kasu_core::Encoder::default();
        encoded.bytes(repo);
        let prefix = ObjectId::hash(&encoded.0);
        format!("repo/blob/v1/{prefix}/{id}").into_bytes()
    }
    pub fn associate_blob(&self, repo: &[u8], id: BlobId) -> Result<()> {
        self.blob_metadata(id)?;
        self.meta
            .put_if_absent(&Self::blob_membership_key(repo, id), &[])
    }
    pub fn has_blob_membership(&self, repo: &[u8], id: BlobId) -> Result<bool> {
        Ok(self
            .meta
            .get(&Self::blob_membership_key(repo, id))?
            .is_some())
    }
    pub fn require_blob_membership(&self, repo: &[u8], id: BlobId) -> Result<()> {
        if !self.has_blob_membership(repo, id)? {
            return Err(Error::NotFound(format!("blob {id} in repository")));
        }
        Ok(())
    }
    /// Key prefixes of every membership index owned by `repo`.
    pub fn membership_prefixes(repo: &[u8]) -> Vec<Vec<u8>> {
        let mut encoded = kasu_core::Encoder::default();
        encoded.bytes(repo);
        let prefix = ObjectId::hash(&encoded.0);
        ["blob", "changeset", "tree"]
            .iter()
            .map(|kind| format!("repo/{kind}/v1/{prefix}/").into_bytes())
            .collect()
    }
    fn membership_key(repo: &[u8], id: ChangesetId) -> Vec<u8> {
        let mut encoded = kasu_core::Encoder::default();
        encoded.bytes(repo);
        let prefix = ObjectId::hash(&encoded.0);
        format!("repo/changeset/v1/{prefix}/{id}").into_bytes()
    }
    pub fn associate_changeset(&self, repo: &[u8], id: ChangesetId) -> Result<()> {
        let cs = self.changeset(id)?;
        for parent in cs.parents {
            self.require_membership(repo, parent)?;
        }
        self.meta
            .put_if_absent(&Self::membership_key(repo, id), &[])
    }
    /// Changesets of `repo` whose hex ID starts with `prefix` (at most `limit`).
    pub fn changesets_with_prefix(
        &self,
        repo: &[u8],
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<ChangesetId>> {
        if prefix.is_empty() || prefix.len() > 64 || !prefix.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid("invalid changeset ID prefix"));
        }
        let mut encoded = kasu_core::Encoder::default();
        encoded.bytes(repo);
        let base = format!("repo/changeset/v1/{}/", ObjectId::hash(&encoded.0));
        let key = format!("{base}{}", prefix.to_ascii_lowercase());
        self.meta
            .scan_prefix(key.as_bytes(), limit)?
            .into_iter()
            .map(|key| {
                let hex = key
                    .strip_prefix(base.as_bytes())
                    .ok_or_else(|| invalid("unexpected membership key"))?;
                let bytes = std::str::from_utf8(hex)
                    .ok()
                    .and_then(|h| {
                        (0..h.len())
                            .step_by(2)
                            .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
                            .collect::<Option<Vec<u8>>>()
                    })
                    .ok_or_else(|| invalid("corrupt membership key"))?;
                Ok(ChangesetId(
                    bytes
                        .try_into()
                        .map_err(|_| invalid("corrupt membership key"))?,
                ))
            })
            .collect()
    }
    pub fn require_membership(&self, repo: &[u8], id: ChangesetId) -> Result<()> {
        if self.meta.get(&Self::membership_key(repo, id))?.is_none() {
            return Err(Error::NotFound(format!("changeset {id} in repository")));
        }
        Ok(())
    }
    pub fn put_blob(&self, raw: &[u8]) -> Result<BlobId> {
        let id = BlobId(self.blobs.put(raw)?.0);
        let metadata = BlobMetadata {
            size: raw.len() as u64,
            git_blob_oid: GitOid::hash("blob", raw),
        };
        self.meta
            .put_if_absent(format!("blob/{id}").as_bytes(), &metadata.encode())?;
        Ok(id)
    }
    pub fn blob_metadata(&self, id: BlobId) -> Result<BlobMetadata> {
        let raw = self
            .meta
            .get(format!("blob/{id}").as_bytes())?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        BlobMetadata::decode(&raw)
    }
    pub fn blob(&self, id: BlobId) -> Result<Vec<u8>> {
        self.blobs.get(ObjectId(id.0))
    }
    pub fn changeset(&self, id: ChangesetId) -> Result<Changeset> {
        let raw = self
            .meta
            .get(format!("changeset/{id}").as_bytes())?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        let value = Changeset::decode(&raw)?;
        if value.id() != id {
            return Err(invalid("corrupt changeset hash"));
        }
        Ok(value)
    }
    // Called only after server-side parent/tree/blob validation.
    pub fn put_changeset(&self, value: &Changeset) -> Result<ChangesetId> {
        value.validate()?;
        let id = value.id();
        self.meta
            .put_if_absent(format!("changeset/{id}").as_bytes(), &value.encode())?;
        Ok(id)
    }
    pub fn tree(&self, id: TreeId) -> Result<Tree> {
        let tree = Tree::decode(&self.blobs.get(ObjectId(id.0))?)?;
        if tree.id() != id {
            return Err(invalid("corrupt tree hash"));
        }
        Ok(tree)
    }
    pub fn put_tree(&self, value: &Tree) -> Result<TreeId> {
        value.validate()?;
        Ok(TreeId(self.blobs.put(&value.encode())?.0))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitRefState {
    pub value: Option<kasu_core::GitRef>,
    pub version: u64,
}
#[derive(Clone, Debug)]
pub struct GitRefUpdate {
    pub name: Vec<u8>,
    pub expected: GitRefState,
    pub new: Option<kasu_core::GitRef>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRefLog {
    pub seq: u64,
    pub name: Vec<u8>,
    pub old: Option<kasu_core::GitRef>,
    pub new: Option<kasu_core::GitRef>,
    pub actor: Vec<u8>,
    pub via: Vec<u8>,
    pub time: i64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitCasResult {
    Applied(Vec<GitRefState>),
    Mismatch { name: Vec<u8>, actual: GitRefState },
}
pub trait GitRefStore: RefStore {
    /// Whether branches and Git-only refs share one CAS/log transaction.
    fn supports_atomic_publication(&self) -> bool {
        false
    }
    // Optional cross-model transaction, used for complete import publication and
    // later atomic pushes involving both branches and tags.
    fn compare_native_and_git(
        &self,
        _repo: &[u8],
        _native: &[RefUpdate],
        _git: &[GitRefUpdate],
        _actor: &[u8],
        _time: i64,
    ) -> Result<RefPublicationResult> {
        Err(invalid(
            "backend does not support atomic native/Git ref publication",
        ))
    }
    fn resolve_git(&self, repo: &[u8], name: &[u8]) -> Result<GitRefState>;
    fn list_git(&self, repo: &[u8], prefix: &[u8]) -> Result<Vec<(Vec<u8>, GitRefState)>>;
    fn compare_git(
        &self,
        repo: &[u8],
        updates: &[GitRefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<GitCasResult>;
    fn tail_git(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<GitRefLog>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefPublicationResult {
    Applied {
        native: Vec<RefState>,
        git: Vec<GitRefState>,
    },
    NativeMismatch {
        name: Vec<u8>,
        actual: RefState,
    },
    GitMismatch {
        name: Vec<u8>,
        actual: GitRefState,
    },
}
