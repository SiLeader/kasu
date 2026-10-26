use kasu_core::{
    Canonical, Decoder, Encoder, Error, RepoKey, RepoName, RepositoryState, Result, invalid,
};
use std::{collections::BTreeMap, sync::Mutex};

/// The synthesized Git commit rule fixed at creation (see 04). Changing the rule
/// for an existing repository would change every advertised Git OID.
pub const GIT_COMMIT_RULE: u32 = 1;
pub const DEFAULT_BRANCH: &[u8] = b"main";
pub const MAX_REPOSITORY_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_FORMER_NAMES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryRecord {
    pub key: RepoKey,
    pub name: RepoName,
    pub state: RepositoryState,
    /// Short branch name: HEAD points to `refs/heads/<default_branch>`.
    pub default_branch: Vec<u8>,
    pub git_commit_rule: u32,
    /// Generation of `config`; changed only through compare-and-swap.
    pub config_version: u64,
    /// Repository-scoped configuration, interpreted by the server.
    pub config: Vec<u8>,
    pub created_by: Vec<u8>,
    pub created_at: i64,
    /// Fingerprint of the creating request, used to replay idempotent creates.
    pub creation_key: Vec<u8>,
    pub deleted_at: i64,
    /// Mutable rows and indexes of a Deleting repository have been removed.
    pub purged: bool,
    /// Earlier primary names, oldest first. Account grants match these as well,
    /// so a rename never removes access; aliases are never matched.
    pub former_names: Vec<RepoName>,
    /// Row version for compare-and-swap; not part of the encoded record.
    pub version: u64,
}
impl RepositoryRecord {
    pub fn new(key: RepoKey, name: RepoName, created_by: &[u8], created_at: i64) -> Self {
        Self {
            key,
            name,
            state: RepositoryState::Active,
            default_branch: DEFAULT_BRANCH.to_vec(),
            git_commit_rule: GIT_COMMIT_RULE,
            config_version: 0,
            config: Vec::new(),
            created_by: created_by.to_vec(),
            created_at,
            creation_key: Vec::new(),
            deleted_at: 0,
            purged: false,
            former_names: Vec::new(),
            version: 0,
        }
    }
    /// The current name followed by earlier primary names.
    pub fn names(&self) -> impl Iterator<Item = &RepoName> {
        std::iter::once(&self.name).chain(self.former_names.iter().rev())
    }
    pub(crate) fn renamed(mut self, name: &RepoName) -> Self {
        let old = std::mem::replace(&mut self.name, name.clone());
        if old.normalized() != name.normalized() {
            self.former_names
                .retain(|n| n.normalized() != old.normalized());
            self.former_names.push(old);
            if self.former_names.len() > MAX_FORMER_NAMES {
                self.former_names.remove(0);
            }
        }
        self
    }
    pub fn head(&self) -> Vec<u8> {
        [b"refs/heads/".as_slice(), &self.default_branch].concat()
    }
    pub fn validate(&self) -> Result<()> {
        if self.default_branch.is_empty()
            || self.default_branch.len() > 255
            || self.default_branch.contains(&0)
        {
            return Err(invalid("invalid default branch"));
        }
        if self.config.len() > MAX_REPOSITORY_CONFIG_BYTES
            || self.created_by.len() > 1024
            || self.creation_key.len() > 256
        {
            return Err(invalid("repository record exceeds limit"));
        }
        if self.purged && self.state != RepositoryState::Deleting {
            return Err(invalid("only deleted repositories can be purged"));
        }
        Ok(())
    }
}
impl Canonical for RepositoryRecord {
    fn encode_to(&self, out: &mut Encoder) {
        out.byte(1);
        out.bytes(self.key.as_bytes());
        out.bytes(self.name.as_str().as_bytes());
        out.byte(self.state.code());
        out.bytes(&self.default_branch);
        out.u32(self.git_commit_rule);
        out.u64(self.config_version);
        out.bytes(&self.config);
        out.bytes(&self.created_by);
        out.i64(self.created_at);
        out.bytes(&self.creation_key);
        out.i64(self.deleted_at);
        out.byte(u8::from(self.purged));
        out.len(self.former_names.len());
        for name in &self.former_names {
            out.bytes(name.as_str().as_bytes());
        }
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        if input.byte()? != 1 {
            return Err(invalid("unsupported repository record version"));
        }
        Ok(Self {
            key: RepoKey::from_bytes(input.bytes()?.to_vec())?,
            name: RepoName::from_bytes(input.bytes()?)?,
            state: RepositoryState::from_code(input.byte()?)?,
            default_branch: input.bytes()?.to_vec(),
            git_commit_rule: input.u32()?,
            config_version: input.u64()?,
            config: input.bytes()?.to_vec(),
            created_by: input.bytes()?.to_vec(),
            created_at: input.i64()?,
            creation_key: input.bytes()?.to_vec(),
            deleted_at: input.i64()?,
            purged: match input.byte()? {
                0 => false,
                1 => true,
                _ => return Err(invalid("invalid repository purge flag")),
            },
            former_names: {
                let count = input.u32()? as usize;
                if count > MAX_FORMER_NAMES {
                    return Err(invalid("too many former repository names"));
                }
                (0..count)
                    .map(|_| RepoName::from_bytes(input.bytes()?))
                    .collect::<Result<_>>()?
            },
            version: 0,
        })
    }
}

/// A row of the name index. Aliases keep a renamed repository reachable
/// under its old name until they expire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryName {
    pub name: String,
    pub key: RepoKey,
    pub alias: bool,
    pub expires_at: Option<i64>,
}
impl RepositoryName {
    pub fn live(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|at| at > now)
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryWrite {
    Applied(RepositoryRecord),
    /// The name or a live alias is already registered.
    NameTaken,
    /// The record changed (or does not exist); the current value is returned.
    Mismatch(Option<RepositoryRecord>),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryEvent {
    pub seq: u64,
    pub key: RepoKey,
    pub event: String,
    pub actor: Vec<u8>,
    pub time: i64,
}

/// The repository registry. Name uniqueness and record updates share one
/// transaction; every change appends to the repository log, which is also a
/// change feed for cache invalidation.
pub trait RepositoryStore: Send + Sync {
    fn create_repository(&self, record: &RepositoryRecord) -> Result<RepositoryWrite>;
    fn repository(&self, key: &RepoKey) -> Result<Option<RepositoryRecord>>;
    /// Resolve a live name or alias. Names of Deleting repositories are released.
    fn resolve_repository_name(&self, name: &RepoName, now: i64) -> Result<Option<RepositoryName>>;
    /// Primary names whose normalized form starts with `prefix`, after `after`.
    fn list_repository_names(
        &self,
        prefix: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<RepositoryName>>;
    /// Every record ordered by key, including Deleting tombstones.
    fn list_repositories(
        &self,
        after: Option<&RepoKey>,
        limit: usize,
    ) -> Result<Vec<RepositoryRecord>>;
    /// Replace a record whose version equals `record.version`. The name and key
    /// are immutable here; entering Deleting releases its names and aliases.
    fn update_repository(
        &self,
        record: &RepositoryRecord,
        event: &str,
        actor: &[u8],
        time: i64,
    ) -> Result<RepositoryWrite>;
    /// Move the primary name. `alias_expires_at` keeps the old name as an alias
    /// (`Some(i64::MAX)` never expires); `None` releases it.
    fn rename_repository(
        &self,
        key: &RepoKey,
        expected_version: u64,
        name: &RepoName,
        alias_expires_at: Option<i64>,
        actor: &[u8],
        time: i64,
    ) -> Result<RepositoryWrite>;
    fn tail_repositories(&self, after: u64, limit: usize) -> Result<Vec<RepositoryEvent>>;
    /// Remove refs, Git refs, Land results and webhook state of a Deleting
    /// repository. The ref logs are audit records and are kept unless requested.
    fn purge_repository_refs(&self, key: &RepoKey, ref_logs: bool) -> Result<()>;
}

pub(crate) fn require_deleting(record: Option<&RepositoryRecord>) -> Result<()> {
    if record.is_none_or(|r| r.state != RepositoryState::Deleting) {
        return Err(invalid("only Deleting repositories can be purged"));
    }
    Ok(())
}
pub(crate) fn check_update(current: &RepositoryRecord, next: &RepositoryRecord) -> Result<()> {
    next.validate()?;
    if current.key != next.key
        || current.name != next.name
        || current.former_names != next.former_names
    {
        return Err(invalid(
            "repository key and name are changed by rename only",
        ));
    }
    if current.git_commit_rule != next.git_commit_rule {
        return Err(invalid("the Git commit rule is fixed at creation"));
    }
    if current.state == RepositoryState::Deleting && next.state != RepositoryState::Deleting {
        return Err(Error::NotFound("repository".into()));
    }
    Ok(())
}

#[derive(Default)]
struct MemoryState {
    records: BTreeMap<RepoKey, RepositoryRecord>,
    names: BTreeMap<String, RepositoryName>,
    log: Vec<RepositoryEvent>,
}
impl MemoryState {
    fn name_free(&self, normalized: &str, now: i64) -> bool {
        self.names.get(normalized).is_none_or(|n| !n.live(now))
    }
    fn append(&mut self, key: &RepoKey, event: &str, actor: &[u8], time: i64) {
        let seq = self.log.len() as u64 + 1;
        self.log.push(RepositoryEvent {
            seq,
            key: key.clone(),
            event: event.into(),
            actor: actor.to_vec(),
            time,
        });
    }
}
/// A process-local registry for tests and single-repository compatibility.
#[derive(Default)]
pub struct MemoryRepositoryStore {
    state: Mutex<MemoryState>,
}
impl MemoryRepositoryStore {
    pub fn new() -> Self {
        Self::default()
    }
    /// A registry holding one repository whose key is its legacy name bytes.
    pub fn legacy(name: &RepoName) -> Result<Self> {
        let store = Self::new();
        let record = RepositoryRecord::new(RepoKey::legacy(name), name.clone(), b"legacy", 0);
        store.create_repository(&record)?;
        Ok(store)
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>> {
        self.state
            .lock()
            .map_err(|_| Error::Storage("repository registry lock poisoned".into()))
    }
}
impl RepositoryStore for MemoryRepositoryStore {
    fn create_repository(&self, record: &RepositoryRecord) -> Result<RepositoryWrite> {
        record.validate()?;
        let mut state = self.lock()?;
        let normalized = record.name.normalized();
        if state.records.contains_key(&record.key)
            || !state.name_free(&normalized, record.created_at)
        {
            return Ok(RepositoryWrite::NameTaken);
        }
        let mut stored = record.clone();
        stored.version = 1;
        state.names.insert(
            normalized.clone(),
            RepositoryName {
                name: normalized,
                key: record.key.clone(),
                alias: false,
                expires_at: None,
            },
        );
        state.records.insert(record.key.clone(), stored.clone());
        state.append(
            &record.key,
            "created",
            &record.created_by,
            record.created_at,
        );
        Ok(RepositoryWrite::Applied(stored))
    }
    fn repository(&self, key: &RepoKey) -> Result<Option<RepositoryRecord>> {
        Ok(self.lock()?.records.get(key).cloned())
    }
    fn resolve_repository_name(&self, name: &RepoName, now: i64) -> Result<Option<RepositoryName>> {
        Ok(self
            .lock()?
            .names
            .get(&name.normalized())
            .filter(|n| n.live(now))
            .cloned())
    }
    fn list_repository_names(
        &self,
        prefix: &str,
        after: &str,
        limit: usize,
    ) -> Result<Vec<RepositoryName>> {
        let state = self.lock()?;
        Ok(state
            .names
            .range::<str, _>((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
            .map(|(_, n)| n)
            .filter(|n| !n.alias && n.name.starts_with(prefix))
            .take(limit)
            .cloned()
            .collect())
    }
    fn list_repositories(
        &self,
        after: Option<&RepoKey>,
        limit: usize,
    ) -> Result<Vec<RepositoryRecord>> {
        let state = self.lock()?;
        Ok(state
            .records
            .values()
            .filter(|r| after.is_none_or(|after| r.key > *after))
            .take(limit)
            .cloned()
            .collect())
    }
    fn update_repository(
        &self,
        record: &RepositoryRecord,
        event: &str,
        actor: &[u8],
        time: i64,
    ) -> Result<RepositoryWrite> {
        let mut state = self.lock()?;
        let Some(current) = state.records.get(&record.key).cloned() else {
            return Ok(RepositoryWrite::Mismatch(None));
        };
        if current.version != record.version {
            return Ok(RepositoryWrite::Mismatch(Some(current)));
        }
        check_update(&current, record)?;
        let mut stored = record.clone();
        stored.version = current.version + 1;
        if stored.state == RepositoryState::Deleting {
            state.names.retain(|_, n| n.key != record.key);
        }
        state.records.insert(record.key.clone(), stored.clone());
        state.append(&record.key, event, actor, time);
        Ok(RepositoryWrite::Applied(stored))
    }
    fn rename_repository(
        &self,
        key: &RepoKey,
        expected_version: u64,
        name: &RepoName,
        alias_expires_at: Option<i64>,
        actor: &[u8],
        time: i64,
    ) -> Result<RepositoryWrite> {
        let mut state = self.lock()?;
        let Some(current) = state.records.get(key).cloned() else {
            return Ok(RepositoryWrite::Mismatch(None));
        };
        if current.version != expected_version || current.state == RepositoryState::Deleting {
            return Ok(RepositoryWrite::Mismatch(Some(current)));
        }
        let normalized = name.normalized();
        let old = current.name.normalized();
        // Renaming to a different spelling of the same name only changes display.
        if normalized != old
            && !state.name_free(&normalized, time)
            && state.names.get(&normalized).is_none_or(|n| n.key != *key)
        {
            return Ok(RepositoryWrite::NameTaken);
        }
        state.names.remove(&old);
        if normalized != old
            && let Some(expires_at) = alias_expires_at
        {
            state.names.insert(
                old.clone(),
                RepositoryName {
                    name: old,
                    key: key.clone(),
                    alias: true,
                    expires_at: (expires_at != i64::MAX).then_some(expires_at),
                },
            );
        }
        state.names.insert(
            normalized.clone(),
            RepositoryName {
                name: normalized,
                key: key.clone(),
                alias: false,
                expires_at: None,
            },
        );
        let mut stored = current.renamed(name);
        stored.version += 1;
        state.records.insert(key.clone(), stored.clone());
        state.append(key, "renamed", actor, time);
        Ok(RepositoryWrite::Applied(stored))
    }
    fn tail_repositories(&self, after: u64, limit: usize) -> Result<Vec<RepositoryEvent>> {
        Ok(self
            .lock()?
            .log
            .iter()
            .filter(|e| e.seq > after)
            .take(limit)
            .cloned()
            .collect())
    }
    fn purge_repository_refs(&self, key: &RepoKey, _ref_logs: bool) -> Result<()> {
        require_deleting(self.lock()?.records.get(key))
    }
}
