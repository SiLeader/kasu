use crate::{
    Backend, RepoPattern,
    read_scope::{ReadScope, below},
    registry::{Repo, RepoContext},
};
use base64::Engine;
use kasu_core::{RepoName, RepoPath};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tonic::Status;

/// Git projection directories readable by a projection account in the
/// repositories matching `repository`.
pub struct ProjectionScope {
    pub repository: RepoPattern,
    pub directory: RepoPath,
}
// Deliberately not Debug: this value contains a credential.
pub struct ProjectionAccount {
    pub name: String,
    pub token: String,
    /// Pre-registry form: directories of the legacy repository, used when
    /// `scopes` is empty.
    pub directories: Vec<RepoPath>,
    pub scopes: Vec<ProjectionScope>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    accounts: Vec<FileAccount>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAccount {
    name: String,
    token_file: PathBuf,
    #[serde(default)]
    directories: Vec<String>,
    #[serde(default)]
    directories_hex: Vec<String>,
    #[serde(default)]
    projections: Vec<FileProjection>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileProjection {
    repository: String,
    #[serde(default)]
    directories: Vec<String>,
    #[serde(default)]
    directories_hex: Vec<String>,
}
fn projection_directories(text: Vec<String>, hex: Vec<String>) -> Result<Vec<RepoPath>, Status> {
    let mut directories = text
        .into_iter()
        .map(|path| RepoPath::new(path.into_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Status::invalid_argument("invalid projection directory"))?;
    for directory in hex {
        let bytes = hex::decode(directory)
            .map_err(|_| Status::invalid_argument("invalid projection directory hex"))?;
        directories.push(
            RepoPath::new(bytes)
                .map_err(|_| Status::invalid_argument("invalid projection directory"))?,
        );
    }
    Ok(directories)
}
pub fn load_projection_accounts(path: &Path) -> Result<Vec<ProjectionAccount>, Status> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|_| Status::invalid_argument("cannot read projection account configuration"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(Status::invalid_argument(
            "projection account configuration exceeds limit",
        ));
    }
    let config: FileConfig = serde_json::from_slice(&bytes)
        .map_err(|_| Status::invalid_argument("invalid projection account configuration"))?;
    if config.accounts.is_empty() || config.accounts.len() > 256 {
        return Err(Status::invalid_argument(
            "expected 1..256 projection accounts",
        ));
    }
    config
        .accounts
        .into_iter()
        .map(|entry| {
            let token_path = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(entry.token_file);
            let token = kasu_object::read_token(&token_path)?;
            let directories = projection_directories(entry.directories, entry.directories_hex)?;
            let mut scopes = Vec::new();
            for projection in entry.projections {
                let repository = RepoPattern::parse(&projection.repository)?;
                for directory in
                    projection_directories(projection.directories, projection.directories_hex)?
                {
                    scopes.push(ProjectionScope {
                        repository: repository.clone(),
                        directory,
                    });
                }
            }
            if !scopes.is_empty() && !directories.is_empty() {
                return Err(Status::invalid_argument(
                    "projections replace account-level directories",
                ));
            }
            Ok(ProjectionAccount {
                name: entry.name,
                token,
                directories,
                scopes,
            })
        })
        .collect()
}

/// The authenticated principal, before any repository is resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Credential {
    /// No credentials are configured: every repository is open.
    Anonymous,
    Operator,
    Projection(usize),
    Account(usize),
    /// The cache → server object hop credential.
    Hop,
    /// A delegate acting as a repository account.
    Delegated(crate::delegate::Delegation),
}
impl Credential {
    /// The repository account whose grants apply.
    pub(crate) fn account(self) -> Option<usize> {
        match self {
            Self::Account(index) => Some(index),
            Self::Delegated(d) => Some(d.account),
            _ => None,
        }
    }
    pub(crate) fn anonymous_delegation(self) -> bool {
        matches!(self, Self::Delegated(d) if d.anonymous)
    }
}
/// What a scoped request may do once its repository is resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Read,
    Write,
    Objects,
}
pub(crate) fn not_found() -> Status {
    Status::not_found("repository not found")
}
impl Backend {
    pub(crate) fn transport_identity(
        &self,
        authorization: Option<&str>,
        objects: bool,
    ) -> Result<Option<crate::admission::Identity>, Status> {
        use crate::admission::Identity;
        Ok(match self.credential(authorization, objects)? {
            Credential::Anonymous => None,
            Credential::Operator => Some(Identity::Operator),
            Credential::Hop => Some(Identity::Hop),
            Credential::Account(index) => Some(Identity::Account(index as u16)),
            Credential::Delegated(d) => Some(Identity::Account(d.account as u16)),
            Credential::Projection(index) => Some(Identity::Projection(index as u16)),
        })
    }

    pub fn with_projection_accounts(
        mut self,
        mut accounts: Vec<ProjectionAccount>,
    ) -> Result<Self, Status> {
        if accounts.is_empty() || accounts.len() > 256 {
            return Err(Status::invalid_argument(
                "expected 1..256 projection accounts",
            ));
        }
        for account in &mut accounts {
            if account.scopes.is_empty() {
                account.scopes = std::mem::take(&mut account.directories)
                    .into_iter()
                    .map(|directory| ProjectionScope {
                        repository: RepoPattern::Legacy,
                        directory,
                    })
                    .collect();
            } else if !account.directories.is_empty() {
                return Err(Status::invalid_argument(
                    "projections replace account-level directories",
                ));
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for (i, account) in accounts.iter().enumerate() {
            kasu_object::bearer(&account.token)?;
            if account.name.is_empty()
                || account.name.len() > 128
                || !names.insert(&account.name)
                || account.scopes.is_empty()
                || account.scopes.len() > 128
                || account
                    .scopes
                    .iter()
                    .any(|s| s.directory.as_bytes().len() > 4096)
                || self.token.as_deref() == Some(&account.token)
                || self.object_token.as_deref() == Some(&account.token)
                || self
                    .repository_accounts
                    .iter()
                    .any(|a| a.token == account.token || a.name == account.name)
                || accounts[..i].iter().any(|a| a.token == account.token)
            {
                return Err(Status::invalid_argument(
                    "invalid or overlapping projection account configuration",
                ));
            }
        }
        self.projection_accounts = std::sync::Arc::new(accounts);
        Ok(self)
    }
    pub(crate) fn authenticate(&self, authorization: Option<&str>) -> Result<Credential, Status> {
        if self.token.is_none()
            && self.projection_accounts.is_empty()
            && self.repository_accounts.is_empty()
        {
            return Ok(Credential::Anonymous);
        }
        let header = authorization.ok_or_else(|| Status::unauthenticated("token required"))?;
        if let Some(credential) = self.delegated(header) {
            return credential;
        }
        if header.len() > 8192 {
            return Err(Status::unauthenticated("invalid credentials"));
        }
        let token = if let Some(value) = header.strip_prefix("Bearer ") {
            value.as_bytes().to_vec()
        } else if let Some(value) = header.strip_prefix("Basic ") {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(value)
                .map_err(|_| Status::unauthenticated("invalid credentials"))?;
            let colon = decoded
                .iter()
                .position(|b| *b == b':')
                .ok_or_else(|| Status::unauthenticated("invalid credentials"))?;
            decoded[colon + 1..].to_vec()
        } else {
            return Err(Status::unauthenticated("unsupported authentication scheme"));
        };
        if self
            .token
            .as_ref()
            .is_some_and(|expected| kasu_object::token_eq(&token, expected.as_bytes()))
        {
            return Ok(Credential::Operator);
        }
        for (index, account) in self.projection_accounts.iter().enumerate() {
            if kasu_object::token_eq(&token, account.token.as_bytes()) {
                return Ok(Credential::Projection(index));
            }
        }
        for (index, account) in self.repository_accounts.iter().enumerate() {
            if kasu_object::token_eq(&token, account.token.as_bytes()) {
                return Ok(Credential::Account(index));
            }
        }
        Err(Status::unauthenticated("invalid token"))
    }
    /// Authenticate for a native endpoint. Objects accept the hop credential;
    /// when one is configured, other callers need directory-restricted reads.
    pub(crate) fn credential(
        &self,
        authorization: Option<&str>,
        objects: bool,
    ) -> Result<Credential, Status> {
        if objects && let Some(token) = &self.object_token {
            let expected = format!("Bearer {token}");
            if authorization
                .is_some_and(|value| kasu_object::token_eq(value.as_bytes(), expected.as_bytes()))
            {
                return Ok(Credential::Hop);
            }
            return match self.authenticate(authorization)? {
                Credential::Account(index)
                    if self.repository_accounts[index].restricted_somewhere() =>
                {
                    Ok(Credential::Account(index))
                }
                _ => Err(Status::unauthenticated("object hop token required")),
            };
        }
        self.authenticate(authorization)
    }
    /// Authenticate a request that will be scoped for `mode`, before the
    /// request body is validated.
    pub(crate) fn precheck(&self, authorization: Option<&str>, mode: Mode) -> Result<(), Status> {
        self.native_credential(authorization, mode).map(|_| ())
    }
    pub(crate) fn authorize(&self, authorization: Option<&str>) -> Result<(), Status> {
        match self.authenticate(authorization)? {
            Credential::Projection(_) => Err(Status::permission_denied(
                "projection credentials require a Git projection endpoint",
            )),
            _ => Ok(()),
        }
    }
    /// Resolve a requested name (or live alias) to a visible repository.
    /// Unknown, invalid, Deleting and pinned-away names are indistinguishable.
    pub(crate) fn resolve(&self, repo: &[u8]) -> Result<Arc<Repo>, Status> {
        let name = RepoName::from_bytes(repo).map_err(|_| not_found())?;
        let repository = self.registry.resolve(&name)?.ok_or_else(not_found)?;
        if self
            .pinned
            .as_ref()
            .is_some_and(|key| *key != repository.record.key)
        {
            return Err(not_found());
        }
        Ok(repository)
    }
    /// Resolve the repository on a blocking worker unless the cache is fresh.
    pub(crate) async fn resolve_async(self: &Arc<Self>, repo: &[u8]) -> Result<Arc<Repo>, Status> {
        if let Ok(name) = RepoName::from_bytes(repo)
            && let Some(cached) = self.registry.cached(&name)
            && self.pinned.is_none()
        {
            return cached.ok_or_else(not_found);
        }
        let state = self.clone();
        let repo = repo.to_vec();
        crate::admission::blocking(move || state.resolve(&repo))
            .await
            .map_err(|_| Status::internal("repository resolution worker failed"))?
    }
    /// Scope to a registered repository with full access, for server-side
    /// workers such as webhook delivery and deletion.
    pub fn for_repository(&self, key: &kasu_core::RepoKey) -> Result<Self, Status> {
        let repository = self.registry.load(key)?.ok_or_else(not_found)?;
        let mut scoped = self.clone();
        scoped.writer_grant = None;
        scoped.read_scope = None;
        scoped.repo = Some(Arc::new(RepoContext {
            requested: repository.record.name.as_str().as_bytes().to_vec(),
            repo: repository,
        }));
        Ok(scoped)
    }
    /// Scope this backend to one repository for a native request.
    pub fn scope(
        &self,
        authorization: Option<&str>,
        repo: &[u8],
        mode: Mode,
    ) -> Result<Self, Status> {
        let credential = self.native_credential(authorization, mode)?;
        let repository = self.resolve(repo)?;
        let mut scoped = self.scope_resolved(credential, repository, repo, mode)?;
        scoped.via = self.delegation_via(authorization).map(Arc::new);
        Ok(scoped)
    }
    pub(crate) async fn scoped(
        self: &Arc<Self>,
        authorization: Option<&str>,
        repo: &[u8],
        mode: Mode,
    ) -> Result<Arc<Self>, Status> {
        let credential = self.native_credential(authorization, mode)?;
        let repository = self.resolve_async(repo).await?;
        let mut scoped = self.scope_resolved(credential, repository, repo, mode)?;
        scoped.via = self.delegation_via(authorization).map(Arc::new);
        Ok(Arc::new(scoped.admit().await?))
    }
    fn native_credential(
        &self,
        authorization: Option<&str>,
        mode: Mode,
    ) -> Result<Credential, Status> {
        match self.credential(authorization, mode == Mode::Objects)? {
            Credential::Projection(_) => Err(Status::permission_denied(
                "projection credentials require a Git projection endpoint",
            )),
            Credential::Delegated(d) if d.anonymous && mode == Mode::Write => {
                Err(Status::permission_denied("anonymous access is read-only"))
            }
            // Independent of the repository, so it reveals nothing about one.
            Credential::Account(account)
            | Credential::Delegated(crate::delegate::Delegation { account, .. })
                if mode == Mode::Write
                    && self.repository_accounts[account]
                        .grants
                        .iter()
                        .all(|g| g.write_policy.is_none()) =>
            {
                Err(Status::permission_denied(
                    "credentials do not authorize writes",
                ))
            }
            credential => Ok(credential),
        }
    }
    fn scope_resolved(
        &self,
        credential: Credential,
        repository: Arc<Repo>,
        requested: &[u8],
        mode: Mode,
    ) -> Result<Self, Status> {
        let mut scoped = self.clone();
        scoped.writer_grant = None;
        scoped.read_scope = None;
        let record = &repository.record;
        match credential {
            Credential::Anonymous | Credential::Operator => {}
            Credential::Hop => {
                if !self
                    .object_repositories
                    .iter()
                    .any(|p| p.matches(record, self.legacy.as_ref()))
                {
                    return Err(not_found());
                }
            }
            Credential::Account(account)
            | Credential::Delegated(crate::delegate::Delegation { account, .. }) => {
                let grant_index = self.repository_accounts[account]
                    .grant_for(record, self.legacy.as_ref())
                    .ok_or_else(not_found)?;
                let grant = self.repository_accounts[account].grant(grant_index);
                if mode == Mode::Objects
                    && self.object_token.is_some()
                    && grant.read_directories.is_none()
                {
                    return Err(Status::unauthenticated("object hop token required"));
                }
                if mode == Mode::Write {
                    if grant.write_policy.is_none() {
                        return Err(Status::permission_denied(
                            "credentials do not authorize writes",
                        ));
                    }
                    scoped.writer_grant = Some((account, grant_index));
                }
                scoped.read_scope = grant
                    .read_directories
                    .as_ref()
                    .map(|dirs| Arc::new(ReadScope::new(record.key.as_bytes(), dirs)));
            }
            Credential::Projection(_) => {
                return Err(Status::permission_denied(
                    "projection credentials require a Git projection endpoint",
                ));
            }
        }
        if mode == Mode::Write && !record.state.writable() {
            return Err(Status::failed_precondition("repository is read-only"));
        }
        scoped.principal = Some(credential);
        scoped.repo = Some(Arc::new(RepoContext {
            repo: repository,
            requested: requested.to_vec(),
        }));
        Ok(scoped)
    }
    /// Scope this backend for a Git request on `repo`, optionally limited to a
    /// projection directory. Read-only repositories still advertise to
    /// receive-pack; their updates are refused through report-status.
    pub(crate) fn scope_git(
        &self,
        authorization: Option<&str>,
        repo: &[u8],
        path: Option<&RepoPath>,
        receiving: bool,
    ) -> Result<Self, Status> {
        let credential = self.authenticate(authorization)?;
        let repository = self.resolve(repo)?;
        let record = &repository.record;
        let mut scoped = self.clone();
        scoped.writer_grant = None;
        scoped.read_scope = None;
        fn readable<'a>(
            path: Option<&RepoPath>,
            mut dirs: impl Iterator<Item = &'a RepoPath>,
        ) -> bool {
            path.is_some_and(|path| dirs.any(|d| below(path.as_bytes(), d.as_bytes())))
        }
        if receiving && credential.anonymous_delegation() {
            return Err(Status::permission_denied("anonymous access is read-only"));
        }
        match credential {
            Credential::Anonymous | Credential::Operator | Credential::Hop => {}
            Credential::Account(account)
            | Credential::Delegated(crate::delegate::Delegation { account, .. }) => {
                let grant_index = self.repository_accounts[account]
                    .grant_for(record, self.legacy.as_ref())
                    .ok_or_else(not_found)?;
                let grant = self.repository_accounts[account].grant(grant_index);
                if let Some(dirs) = &grant.read_directories {
                    if receiving || !readable(path, dirs.iter()) {
                        return Err(Status::permission_denied(
                            "restricted account requires a readable Git projection",
                        ));
                    }
                } else if receiving && grant.write_policy.is_none() {
                    return Err(Status::permission_denied("account is read-only"));
                }
                if receiving {
                    scoped.writer_grant = Some((account, grant_index));
                }
            }
            Credential::Projection(account) => {
                let mut dirs = self.projection_accounts[account]
                    .scopes
                    .iter()
                    .filter(|s| s.repository.matches(record, self.legacy.as_ref()))
                    .map(|s| &s.directory)
                    .peekable();
                if dirs.peek().is_none() {
                    return Err(not_found());
                }
                if receiving || !readable(path, dirs) {
                    return Err(Status::permission_denied(
                        "projection scope does not authorize this operation",
                    ));
                }
            }
        }
        scoped.principal = Some(credential);
        scoped.repo = Some(Arc::new(RepoContext {
            repo: repository,
            requested: repo.to_vec(),
        }));
        Ok(scoped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kasu_git::ProjectionId;
    use kasu_store::{FsBlobStore, SqliteStore};
    fn backend(path: &Path, token: Option<&str>) -> Backend {
        let meta = Arc::new(SqliteStore::memory().unwrap());
        Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            Arc::new(FsBlobStore::open(path).unwrap()),
            meta,
            token.map(str::to_owned),
        )
    }
    fn account(token: &str) -> ProjectionAccount {
        ProjectionAccount {
            name: "reader".into(),
            token: token.into(),
            directories: vec![RepoPath::new(b"deploy".as_slice()).unwrap()],
            scopes: vec![],
        }
    }
    #[test]
    fn projection_only_configuration_closes_anonymous_and_native_access() {
        let dir = tempfile::tempdir().unwrap();
        let state = backend(dir.path(), None)
            .with_projection_accounts(vec![account("reader-token")])
            .unwrap();
        assert_eq!(
            state.authenticate(None).err().unwrap().code(),
            tonic::Code::Unauthenticated
        );
        assert_eq!(
            state
                .authorize(Some("Bearer reader-token"))
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            state
                .authenticate(Some("Bearer wrong"))
                .err()
                .unwrap()
                .code(),
            tonic::Code::Unauthenticated
        );
        let git = |path: &[u8], receiving| {
            let projection = ProjectionId::path(b"repo".as_slice(), RepoPath::new(path).unwrap());
            state.scope_git(
                Some("Bearer reader-token"),
                &projection.repo,
                projection.path.as_ref(),
                receiving,
            )
        };
        for path in [b"deploy".as_slice(), b"deploy/nested", b"deploy/\xff"] {
            git(path, false).unwrap();
            assert!(git(path, true).is_err());
        }
        for path in [b"deployment".as_slice(), b"private"] {
            assert!(git(path, false).is_err());
        }
        // Other repositories are invisible rather than forbidden.
        assert_eq!(
            state
                .scope_git(Some("Bearer reader-token"), b"other", None, false)
                .err()
                .unwrap()
                .code(),
            tonic::Code::NotFound
        );
    }
    #[test]
    fn account_and_hop_credentials_cannot_overlap_in_either_builder_order() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            backend(dir.path(), Some("admin"))
                .with_projection_accounts(vec![account("admin")])
                .is_err()
        );
        assert!(
            backend(dir.path(), Some("admin"))
                .with_object_token("hop".into())
                .unwrap()
                .with_projection_accounts(vec![account("hop")])
                .is_err()
        );
        assert!(
            backend(dir.path(), Some("admin"))
                .with_projection_accounts(vec![account("hop")])
                .unwrap()
                .with_object_token("hop".into())
                .is_err()
        );
        let mut duplicate = account("reader");
        duplicate.name = "other".into();
        assert!(
            backend(dir.path(), None)
                .with_projection_accounts(vec![account("reader"), duplicate])
                .is_err()
        );
        assert!(
            backend(dir.path(), None)
                .with_projection_accounts(Vec::new())
                .is_err()
        );
    }
    #[test]
    fn file_configuration_is_strict_relative_and_supports_byte_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("reader.pat"), b"reader-token\n").unwrap();
        let path = dir.path().join("accounts.json");
        std::fs::write(&path,br#"{"accounts":[{"name":"reader","token_file":"reader.pat","directories":["deploy"],"directories_hex":["ff"]}]}"#).unwrap();
        let accounts = load_projection_accounts(&path).unwrap();
        assert_eq!(accounts[0].directories[1].as_bytes(), b"\xff");
        backend(&dir.path().join("objects"), Some("admin"))
            .with_projection_accounts(accounts)
            .unwrap();
        std::fs::write(&path,br#"{"accounts":[{"name":"reader","token_file":"reader.pat","projections":[{"repository":"team/*","directories":["deploy"]}]}]}"#).unwrap();
        let accounts = load_projection_accounts(&path).unwrap();
        assert_eq!(
            accounts[0].scopes[0].repository,
            RepoPattern::parse("team/*").unwrap()
        );
        for invalid in [br#"{"accounts":[],"token":"secret-material"}"#.as_slice(),br#"{"accounts":[{"name":"r","token_file":"reader.pat","directories":["../private"]}]}"#,br#"{"accounts":[{"name":"r","token_file":"reader.pat","directories_hex":["ff0"]}]}"#,br#"{"accounts":[{"name":"r","token_file":"reader.pat","directories":["a"],"projections":[{"repository":"x","directories":["a"]}]}]}"#] {
            std::fs::write(&path,invalid).unwrap();
            let error=load_projection_accounts(&path).err().unwrap();
            assert!(!error.message().contains("secret-material"));
            assert!(!error.message().contains("reader-token"));
        }
    }
}
