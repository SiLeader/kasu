use kasu_core::{RepoKey, RepoName, RepositoryState};
use kasu_store::{RepositoryRecord, RepositoryStore};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tonic::Status;

const MAX_CACHED_NAMES: usize = 65_536;

/// Repository-scoped configuration stored in the registry record. Every value
/// is bounded by the corresponding server-wide setting.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepositoryConfig {
    /// Intersected with the server policy and the account's grant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_policy: Option<crate::WritePolicy>,
    /// Push webhook endpoints notified for this repository's ref changes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<crate::WebhookEndpoint>,
    #[serde(skip_serializing_if = "RepositoryLimits::is_empty")]
    pub limits: RepositoryLimits,
}
/// Per-repository admission, capped by `ServerLimits::per_repository_*`.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepositoryLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<usize>,
}
impl RepositoryLimits {
    fn is_empty(&self) -> bool {
        self.active.is_none() && self.pending.is_none()
    }
}
impl RepositoryConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self, Status> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_slice(bytes)
            .map_err(|_| Status::internal("invalid stored repository configuration"))
    }
    /// Parse untrusted configuration (Admin API or repositories file).
    pub fn parse_request(bytes: &[u8]) -> Result<Self, Status> {
        if bytes.len() > kasu_store::MAX_REPOSITORY_CONFIG_BYTES {
            return Err(Status::invalid_argument(
                "repository configuration exceeds limit",
            ));
        }
        let config: Self = if bytes.is_empty() {
            Self::default()
        } else {
            serde_json::from_slice(bytes)
                .map_err(|_| Status::invalid_argument("invalid repository configuration"))?
        };
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<(), Status> {
        if let Some(policy) = &self.write_policy {
            crate::accounts::validate_policy(policy)?;
        }
        crate::webhook::validate_endpoints(&self.webhooks, true)?;
        if self.limits.active.is_some_and(|n| !(1..=4096).contains(&n))
            || self
                .limits
                .pending
                .is_some_and(|n| !(1..=65536).contains(&n))
        {
            return Err(Status::invalid_argument("invalid repository limits"));
        }
        Ok(())
    }
    /// The stored form: empty for the default configuration.
    pub fn encode(&self) -> Vec<u8> {
        if *self == Self::default() {
            Vec::new()
        } else {
            serde_json::to_vec(self).expect("repository configuration serializes")
        }
    }
}

/// A resolved, visible repository.
pub(crate) struct Repo {
    pub record: RepositoryRecord,
    pub config: RepositoryConfig,
}
pub(crate) struct RepoContext {
    pub repo: Arc<Repo>,
    /// The name bytes the request used, which may be an alias.
    pub requested: Vec<u8>,
}
type Entry = (Instant, Option<Arc<Repo>>);

/// Registry reads behind a short TTL. The repository log invalidates entries
/// as soon as a node observes it; ref transactions re-check the state, so a
/// stale entry cannot land a write on a repository that left Active.
pub struct Registry {
    store: Arc<dyn RepositoryStore>,
    ttl: Duration,
    entries: Mutex<HashMap<String, Entry>>,
    cursor: AtomicU64,
}
pub(crate) fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}
impl Registry {
    pub fn new(store: Arc<dyn RepositoryStore>, ttl: Duration) -> Self {
        Self {
            store,
            ttl,
            entries: Mutex::new(HashMap::new()),
            cursor: AtomicU64::new(0),
        }
    }
    pub fn store(&self) -> &Arc<dyn RepositoryStore> {
        &self.store
    }
    pub(crate) fn cached(&self, name: &RepoName) -> Option<Option<Arc<Repo>>> {
        let entries = self.entries.lock().ok()?;
        let (at, value) = entries.get(&name.normalized())?;
        (at.elapsed() < self.ttl).then(|| value.clone())
    }
    /// Resolve a name or live alias to a visible (not Deleting) repository.
    /// This may read the registry and must run on a blocking worker.
    pub(crate) fn resolve(&self, name: &RepoName) -> Result<Option<Arc<Repo>>, Status> {
        if let Some(value) = self.cached(name) {
            return Ok(value);
        }
        let value = match self
            .store
            .resolve_repository_name(name, now_seconds())
            .map_err(crate::backend::status)?
        {
            None => None,
            Some(entry) => self.load(&entry.key)?,
        };
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| Status::internal("registry cache lock failed"))?;
        if entries.len() >= MAX_CACHED_NAMES {
            entries.clear();
        }
        entries.insert(name.normalized(), (Instant::now(), value.clone()));
        Ok(value)
    }
    pub(crate) fn load(&self, key: &RepoKey) -> Result<Option<Arc<Repo>>, Status> {
        let Some(record) = self.store.repository(key).map_err(crate::backend::status)? else {
            return Ok(None);
        };
        if record.state == RepositoryState::Deleting {
            return Ok(None);
        }
        let config = RepositoryConfig::parse(&record.config)?;
        Ok(Some(Arc::new(Repo { record, config })))
    }
    /// Forget a repository and every negative entry, e.g. after a local write.
    pub fn invalidate(&self, key: &RepoKey) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|_, (_, value)| value.as_ref().is_some_and(|r| r.record.key != *key));
        }
    }
    /// Apply new repository log events to the cache; returns how many were read.
    pub fn refresh(&self) -> Result<usize, Status> {
        let mut total = 0;
        loop {
            let after = self.cursor.load(Ordering::Acquire);
            let events = self
                .store
                .tail_repositories(after, 1024)
                .map_err(crate::backend::status)?;
            let Some(last) = events.last().map(|e| e.seq) else {
                return Ok(total);
            };
            if after == 0 {
                // Entries cached before the first pass may predate any event.
                if let Ok(mut entries) = self.entries.lock() {
                    entries.clear();
                }
            } else {
                for event in &events {
                    self.invalidate(&event.key);
                }
            }
            total += events.len();
            self.cursor.store(last, Ordering::Release);
        }
    }
    /// Follow the repository log until shutdown.
    pub async fn watch(
        self: Arc<Self>,
        interval: Duration,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            let registry = self.clone();
            match crate::admission::blocking(move || registry.refresh()).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(code=?error.code(),"repository log refresh failed");
                    if let Ok(mut entries) = self.entries.lock() {
                        entries.clear();
                    }
                }
                Err(_) => tracing::error!("repository log worker failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => return,
            }
        }
    }
}
