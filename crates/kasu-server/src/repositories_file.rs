use crate::{RepositoryConfig, WebhookEndpoint, WritePolicy, registry::RepositoryLimits};
use kasu_core::{RepoKey, RepoName, RepositoryState};
use kasu_store::{RepositoryRecord, RepositoryStore, RepositoryWrite};
use std::path::Path;
use tonic::Status;

/// Which source owns repository configuration once the Admin API exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigSource {
    /// `KASU_REPOSITORIES_FILE` is authoritative: the file overwrites registry
    /// configuration and Admin API configuration updates are refused.
    File,
    /// The registry is authoritative: the file only creates missing repositories.
    Registry,
}
impl ConfigSource {
    pub fn from_env() -> Result<Self, Status> {
        match std::env::var("KASU_REPOSITORIES_SOURCE").as_deref() {
            Err(std::env::VarError::NotPresent) | Ok("file") => Ok(Self::File),
            Ok("registry") => Ok(Self::Registry),
            _ => Err(Status::invalid_argument(
                "KASU_REPOSITORIES_SOURCE must be file or registry",
            )),
        }
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    repositories: Vec<Entry>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    write_policy: Option<WritePolicy>,
    #[serde(default)]
    webhooks: Vec<WebhookEndpoint>,
    #[serde(default)]
    limits: RepositoryLimits,
}
/// One declared repository.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclaredRepository {
    pub name: RepoName,
    pub default_branch: Vec<u8>,
    pub config: RepositoryConfig,
}
pub fn load_repositories_file(path: &Path) -> Result<Vec<DeclaredRepository>, Status> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|_| Status::invalid_argument("cannot read repositories file"))?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(Status::invalid_argument("repositories file exceeds limit"));
    }
    let file: File = serde_json::from_slice(&bytes)
        .map_err(|_| Status::invalid_argument("invalid repositories file"))?;
    if file.repositories.len() > 65_536 {
        return Err(Status::invalid_argument("too many declared repositories"));
    }
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut names = std::collections::BTreeSet::new();
    file.repositories
        .into_iter()
        .map(|entry| {
            let name = RepoName::new(entry.name)
                .map_err(|_| Status::invalid_argument("invalid declared repository name"))?;
            if !names.insert(name.normalized()) {
                return Err(Status::invalid_argument("duplicate declared repository"));
            }
            let default_branch = entry.default_branch.unwrap_or_else(|| "main".into());
            kasu_git::validate_branch_name(format!("refs/heads/{default_branch}").as_bytes())
                .map_err(|_| Status::invalid_argument("invalid declared default branch"))?;
            let mut webhooks = entry.webhooks;
            // Secret paths follow the convention of the other configuration files.
            for endpoint in &mut webhooks {
                if let Some(secret) = &mut endpoint.secret_file
                    && secret.is_relative()
                {
                    *secret = base.join(&*secret);
                }
            }
            let config = RepositoryConfig {
                write_policy: entry.write_policy,
                webhooks,
                limits: entry.limits,
            };
            config.validate()?;
            Ok(DeclaredRepository {
                name,
                default_branch: default_branch.into_bytes(),
                config,
            })
        })
        .collect()
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Registered repositories missing from the file; never deleted by sync.
    pub undeclared: usize,
    /// Changes that lost a race with another writer; retried on the next sync.
    pub conflicts: usize,
}
/// Bring the registry in line with the declaration. Repositories absent from
/// the file are reported, never removed.
pub fn sync_repositories(
    store: &dyn RepositoryStore,
    declared: &[DeclaredRepository],
    source: ConfigSource,
    now: i64,
) -> Result<SyncReport, Status> {
    let status = crate::backend::status;
    let mut report = SyncReport::default();
    let mut seen = std::collections::BTreeSet::new();
    for entry in declared {
        let config = entry.config.encode();
        let existing = match store
            .resolve_repository_name(&entry.name, now)
            .map_err(status)?
        {
            Some(name) if name.alias => {
                tracing::warn!(repository=%entry.name,"declared name is an alias of a renamed repository; skipping");
                report.conflicts += 1;
                continue;
            }
            Some(name) => store.repository(&name.key).map_err(status)?,
            None => None,
        };
        let Some(mut record) = existing else {
            let mut record = RepositoryRecord::new(
                RepoKey::generate().map_err(status)?,
                entry.name.clone(),
                b"repositories-file",
                now,
            );
            record.default_branch = entry.default_branch.clone();
            record.config = config;
            record.config_version = 1;
            match store.create_repository(&record).map_err(status)? {
                RepositoryWrite::Applied(record) => {
                    seen.insert(record.key);
                    report.created += 1;
                }
                _ => report.conflicts += 1,
            }
            continue;
        };
        seen.insert(record.key.clone());
        if source == ConfigSource::Registry
            || (record.config == config && record.default_branch == entry.default_branch)
        {
            report.unchanged += 1;
            continue;
        }
        record.config = config;
        record.default_branch = entry.default_branch.clone();
        record.config_version += 1;
        match store
            .update_repository(&record, "config:file", b"repositories-file", now)
            .map_err(status)?
        {
            RepositoryWrite::Applied(_) => report.updated += 1,
            _ => report.conflicts += 1,
        }
    }
    let mut after = None;
    loop {
        let page = store
            .list_repositories(after.as_ref(), 256)
            .map_err(status)?;
        let Some(last) = page.last() else { break };
        after = Some(last.key.clone());
        for record in page {
            if record.state != RepositoryState::Deleting && !seen.contains(&record.key) {
                report.undeclared += 1;
            }
        }
    }
    if report.undeclared > 0 {
        tracing::warn!(
            count = report.undeclared,
            "registered repositories are not declared in KASU_REPOSITORIES_FILE"
        );
    }
    Ok(report)
}
