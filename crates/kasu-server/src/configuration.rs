use crate::Backend;
use kasu_core::{RepoKey, RepoName, RepositoryState};
use kasu_store::{DatabaseStore, RepositoryRecord, RepositoryStore, RepositoryWrite, Storage};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct ConfiguredBackend {
    pub backend: Backend,
    pub database: Arc<dyn DatabaseStore>,
    /// The repository named by the pre-registry startup argument, if any.
    pub legacy: Option<RepositoryRecord>,
}
/// Register an existing single-repository installation. Its key is the
/// repository name bytes, so refs, membership, Git OIDs, Derived Data and
/// webhook cursors keep their stored keys. Running it again is a no-op.
pub fn migrate_registry(
    store: &dyn RepositoryStore,
    name: &RepoName,
    actor: &[u8],
) -> Result<RepositoryRecord, BoxError> {
    let key = RepoKey::legacy(name);
    let now = crate::registry::now_seconds();
    if let Some(entry) = store.resolve_repository_name(name, now)? {
        return match store.repository(&entry.key)? {
            Some(record) if entry.key == key => Ok(record),
            _ => Err("the repository name is registered to another repository".into()),
        };
    }
    if let Some(record) = store.repository(&key)? {
        // A renamed legacy repository is already migrated; a deleted key must
        // never come back, or its former refs and indexes would reappear.
        return if record.state == RepositoryState::Deleting {
            Err("this legacy repository was deleted and cannot be registered again".into())
        } else {
            Ok(record)
        };
    }
    let record = RepositoryRecord::new(key, name.clone(), actor, now);
    match store.create_repository(&record)? {
        RepositoryWrite::Applied(record) => Ok(record),
        _ => Err("the repository name is already registered".into()),
    }
}
/// Resolve the startup argument. An empty registry registers it as a legacy
/// repository (compatibility for one release); otherwise it must exist.
pub fn legacy_repository(
    store: &dyn RepositoryStore,
    name: &RepoName,
) -> Result<RepositoryRecord, BoxError> {
    if let Some(entry) = store.resolve_repository_name(name, crate::registry::now_seconds())?
        && let Some(record) = store.repository(&entry.key)?
    {
        return Ok(record);
    }
    if store.list_repositories(None, 1)?.is_empty() {
        tracing::warn!(repository=%name,"registry is empty; registering the startup repository as a legacy repository");
        return migrate_registry(store, name, b"compat");
    }
    Err(format!(
        "repository {name} is not registered; run `kasu-server migrate-registry <kasu-directory> {name}`"
    )
    .into())
}
pub fn load_backend(
    root: &Path,
    repository: Option<Vec<u8>>,
) -> Result<ConfiguredBackend, BoxError> {
    let Storage { database, blobs } = Storage::from_env(root)?;
    let token = match (
        std::env::var_os("KASU_TOKEN_FILE"),
        std::env::var("KASU_TOKEN"),
    ) {
        (Some(_), Ok(_)) => return Err("use only one of KASU_TOKEN and KASU_TOKEN_FILE".into()),
        (Some(path), Err(std::env::VarError::NotPresent)) => {
            Some(kasu_object::read_token(&PathBuf::from(path))?)
        }
        (None, Ok(value)) if !value.is_empty() => {
            kasu_object::bearer(&value)?;
            Some(value)
        }
        (None, Ok(_)) => return Err("KASU_TOKEN must not be empty".into()),
        (None, Err(std::env::VarError::NotPresent)) => None,
        (_, Err(_)) => return Err("KASU_TOKEN must be UTF-8".into()),
    };
    let legacy = repository
        .map(|bytes| {
            let name = RepoName::from_bytes(&bytes)?;
            legacy_repository(database.as_ref(), &name)
        })
        .transpose()?;
    let mut backend = Backend::with_registry(
        database.clone(),
        blobs,
        database.clone(),
        database.clone(),
        token,
    );
    if let Some(record) = &legacy {
        backend = backend.with_legacy_repository(record.key.clone());
    }
    if let Some(ttl) = std::env::var_os("KASU_REGISTRY_CACHE_MS") {
        let ttl: u64 = ttl
            .to_str()
            .and_then(|v| v.parse().ok())
            .filter(|v| (1..=60_000).contains(v))
            .ok_or("KASU_REGISTRY_CACHE_MS must be 1..60000")?;
        backend = backend.with_registry_ttl(std::time::Duration::from_millis(ttl));
    }
    if let Some(path) = std::env::var_os("KASU_WRITE_POLICY_FILE") {
        backend = backend.with_write_policy(serde_json::from_slice(&std::fs::read(path)?)?);
    }
    if let Some(path) = std::env::var_os("KASU_OBJECT_TOKEN_FILE") {
        backend = backend.with_object_token(kasu_object::read_token(&PathBuf::from(path))?)?;
    }
    if let Some(patterns) = std::env::var_os("KASU_OBJECT_REPOSITORIES") {
        let patterns = patterns
            .to_str()
            .ok_or("KASU_OBJECT_REPOSITORIES must be UTF-8")?
            .split(',')
            .map(|p| crate::RepoPattern::parse(p.trim()))
            .collect::<Result<Vec<_>, _>>()?;
        backend = backend.with_object_repositories(patterns)?;
    }
    if let Some(path) = std::env::var_os("KASU_PROJECTION_ACCOUNTS_FILE") {
        backend = backend
            .with_projection_accounts(crate::load_projection_accounts(&PathBuf::from(path))?)?;
    }
    if let Some(path) = std::env::var_os("KASU_ACCOUNTS_FILE") {
        let path = PathBuf::from(path);
        backend = backend.with_repository_accounts(crate::load_repository_accounts(&path)?)?;
        backend = backend.with_delegates(crate::load_delegates(&path)?)?;
    }
    if let Some(path) = std::env::var_os("KASU_BINDING_KEY_FILE") {
        backend = backend.with_binding_key(std::fs::read(path)?)?;
    } else if !backend.delegates.is_empty() {
        tracing::warn!(
            "KASU_BINDING_KEY_FILE is unset; Web UI sessions end when this server restarts and differ between replicas"
        );
    }
    backend = backend.with_reviews(database.clone());
    if let Some(path) = std::env::var_os("KASU_SERVER_LIMITS_FILE") {
        backend = backend.with_limits(crate::ServerLimits::load(&PathBuf::from(path))?)?;
    }
    let seconds = |name: &str| -> Result<Option<u64>, BoxError> {
        std::env::var_os(name)
            .map(|v| {
                v.to_str()
                    .and_then(|v| v.parse().ok())
                    .filter(|v| *v <= 100 * 365 * 24 * 3600)
                    .ok_or_else(|| format!("{name} must be a number of seconds").into())
            })
            .transpose()
    };
    let mut lifecycle = crate::Lifecycle::default();
    if let Some(ttl) = seconds("KASU_REPOSITORY_ALIAS_TTL_SECS")? {
        lifecycle.alias_ttl = (ttl > 0).then(|| std::time::Duration::from_secs(ttl));
    }
    if let Some(retention) = seconds("KASU_DELETED_REF_LOG_RETENTION_SECS")? {
        lifecycle.ref_log_retention = std::time::Duration::from_secs(retention);
    }
    if let Some(dir) = std::env::var_os("KASU_WEBHOOK_SECRET_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            return Err("KASU_WEBHOOK_SECRET_DIR must be absolute".into());
        }
        lifecycle.webhook_secret_dir = Some(dir);
    }
    if std::env::var_os("KASU_REPOSITORIES_FILE").is_some() {
        lifecycle.config_source = Some(crate::ConfigSource::from_env()?);
    }
    backend = backend.with_lifecycle(lifecycle);
    if let Some(names) = std::env::var_os("KASU_METRICS_REPOSITORIES") {
        let names = names
            .to_str()
            .ok_or("KASU_METRICS_REPOSITORIES must be UTF-8")?
            .split(',')
            .filter(|n| !n.trim().is_empty())
            .map(|n| RepoName::new(n.trim()))
            .collect::<Result<Vec<_>, _>>()?;
        backend = backend.with_repository_metrics(&names)?;
    }
    Ok(ConfiguredBackend {
        backend,
        database,
        legacy,
    })
}
