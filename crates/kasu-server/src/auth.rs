use crate::Backend;
use base64::Engine;
use kasu_core::RepoPath;
use kasu_git::ProjectionId;
use std::path::{Path, PathBuf};
use tonic::Status;

// Deliberately not Debug: this value contains a credential.
pub struct ProjectionAccount {
    pub name: String,
    pub token: String,
    pub directories: Vec<RepoPath>,
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
            let mut directories = entry
                .directories
                .into_iter()
                .map(|path| RepoPath::new(path.into_bytes()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| Status::invalid_argument("invalid projection directory"))?;
            for directory in entry.directories_hex {
                let bytes = hex::decode(directory)
                    .map_err(|_| Status::invalid_argument("invalid projection directory hex"))?;
                directories.push(
                    RepoPath::new(bytes)
                        .map_err(|_| Status::invalid_argument("invalid projection directory"))?,
                );
            }
            Ok(ProjectionAccount {
                name: entry.name,
                token,
                directories,
            })
        })
        .collect()
}

pub(crate) enum Access<'a> {
    Repository,
    Projection(&'a [RepoPath]),
    Account(&'a crate::RepositoryAccount),
}
impl Access<'_> {
    pub(crate) fn check_git(
        &self,
        projection: &ProjectionId,
        receiving: bool,
    ) -> Result<(), Status> {
        match self {
            Self::Repository => Ok(()),
            Self::Account(a) if a.read_directories.is_some() => {
                if !receiving
                    && projection.path.as_ref().is_some_and(|p| {
                        a.read_directories
                            .as_ref()
                            .unwrap()
                            .iter()
                            .any(|d| crate::read_scope::below(p.as_bytes(), d.as_bytes()))
                    })
                {
                    Ok(())
                } else {
                    Err(Status::permission_denied(
                        "restricted account requires a readable Git projection",
                    ))
                }
            }
            Self::Account(a) if !receiving || a.write_policy.is_some() => Ok(()),
            Self::Account(_) => Err(Status::permission_denied("account is read-only")),
            Self::Projection(directories) => {
                if !receiving
                    && projection.path.as_ref().is_some_and(|path| {
                        directories.iter().any(|allowed| {
                            path == allowed
                                || path
                                    .as_bytes()
                                    .strip_prefix(allowed.as_bytes())
                                    .is_some_and(|suffix| suffix.starts_with(b"/"))
                        })
                    })
                {
                    Ok(())
                } else {
                    Err(Status::permission_denied(
                        "projection scope does not authorize this operation",
                    ))
                }
            }
        }
    }
}
impl Backend {
    pub(crate) fn transport_identity(
        &self,
        authorization: Option<&str>,
        objects: bool,
    ) -> Result<Option<crate::admission::Identity>, Status> {
        use crate::admission::Identity;
        if objects && let Some(token) = &self.object_token {
            let expected = format!("Bearer {token}");
            if authorization
                .is_some_and(|value| kasu_object::token_eq(value.as_bytes(), expected.as_bytes()))
            {
                return Ok(Some(Identity::Hop));
            }
            if !matches!(self.authenticate(authorization), Ok(Access::Account(a)) if a.read_directories.is_some())
            {
                return Err(Status::unauthenticated("object hop token required"));
            }
        }
        Ok(match self.authenticate(authorization)? {
            Access::Repository => self.token.as_ref().map(|_| Identity::Operator),
            Access::Account(account) => Some(Identity::Account(
                self.repository_accounts
                    .iter()
                    .position(|a| std::ptr::eq(a, account))
                    .ok_or_else(|| Status::internal("account identity missing"))?
                    as u16,
            )),
            Access::Projection(directories) => Some(Identity::Projection(
                self.projection_accounts
                    .iter()
                    .position(|a| std::ptr::eq(a.directories.as_slice(), directories))
                    .ok_or_else(|| Status::internal("account identity missing"))?
                    as u16,
            )),
        })
    }

    pub fn with_projection_accounts(
        mut self,
        accounts: Vec<ProjectionAccount>,
    ) -> Result<Self, Status> {
        if accounts.is_empty() || accounts.len() > 256 {
            return Err(Status::invalid_argument(
                "expected 1..256 projection accounts",
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for (i, account) in accounts.iter().enumerate() {
            kasu_object::bearer(&account.token)?;
            if account.name.is_empty()
                || account.name.len() > 128
                || !names.insert(&account.name)
                || account.directories.is_empty()
                || account.directories.len() > 128
                || account
                    .directories
                    .iter()
                    .any(|p| p.as_bytes().len() > 4096)
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
    pub(crate) fn authenticate(&self, authorization: Option<&str>) -> Result<Access<'_>, Status> {
        if self.token.is_none()
            && self.projection_accounts.is_empty()
            && self.repository_accounts.is_empty()
        {
            return Ok(Access::Repository);
        }
        let header = authorization.ok_or_else(|| Status::unauthenticated("token required"))?;
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
            return Ok(Access::Repository);
        }
        for account in self.projection_accounts.iter() {
            if kasu_object::token_eq(&token, account.token.as_bytes()) {
                return Ok(Access::Projection(&account.directories));
            }
        }
        for account in self.repository_accounts.iter() {
            if kasu_object::token_eq(&token, account.token.as_bytes()) {
                return Ok(Access::Account(account));
            }
        }
        Err(Status::unauthenticated("invalid token"))
    }
    pub(crate) fn authorize(&self, authorization: Option<&str>) -> Result<(), Status> {
        match self.authenticate(authorization)? {
            Access::Repository | Access::Account(_) => Ok(()),
            Access::Projection(_) => Err(Status::permission_denied(
                "projection credentials require a Git projection endpoint",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kasu_store::{FsBlobStore, SqliteStore};
    use std::sync::Arc;
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
        for path in [b"deploy".as_slice(), b"deploy/nested", b"deploy/\xff"] {
            let projection = ProjectionId::path(b"repo".as_slice(), RepoPath::new(path).unwrap());
            state
                .authenticate(Some("Bearer reader-token"))
                .unwrap()
                .check_git(&projection, false)
                .unwrap();
            assert!(
                state
                    .authenticate(Some("Bearer reader-token"))
                    .unwrap()
                    .check_git(&projection, true)
                    .is_err()
            );
        }
        for path in [b"deployment".as_slice(), b"private"] {
            let projection = ProjectionId::path(b"repo".as_slice(), RepoPath::new(path).unwrap());
            assert!(
                state
                    .authenticate(Some("Bearer reader-token"))
                    .unwrap()
                    .check_git(&projection, false)
                    .is_err()
            );
        }
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
        for invalid in [br#"{"accounts":[],"token":"secret-material"}"#.as_slice(),br#"{"accounts":[{"name":"r","token_file":"reader.pat","directories":["../private"]}]}"#,br#"{"accounts":[{"name":"r","token_file":"reader.pat","directories_hex":["ff0"]}]}"#] {
            std::fs::write(&path,invalid).unwrap();
            let error=load_projection_accounts(&path).err().unwrap();
            assert!(!error.message().contains("secret-material"));
            assert!(!error.message().contains("reader-token"));
        }
    }
}
