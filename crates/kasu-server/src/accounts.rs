use crate::{Backend, WritePolicy, auth::Access};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tonic::Status;

// A credential is deliberately not Debug. None grants repository reads only.
pub struct RepositoryAccount {
    pub name: String,
    pub token: String,
    pub write_policy: Option<WritePolicy>,
    pub read_directories: Option<Vec<kasu_core::RepoPath>>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    accounts: Vec<AccountFile>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountFile {
    name: String,
    token_file: PathBuf,
    #[serde(default)]
    write_policy: Option<WritePolicy>,
    #[serde(default)]
    read_directories: Option<Vec<String>>,
    #[serde(default)]
    read_directories_hex: Option<Vec<String>>,
}
pub fn load_repository_accounts(path: &Path) -> Result<Vec<RepositoryAccount>, Status> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|_| Status::invalid_argument("cannot read repository account configuration"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(Status::invalid_argument(
            "repository account configuration exceeds limit",
        ));
    }
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|_| Status::invalid_argument("invalid repository account configuration"))?;
    if config.accounts.is_empty() || config.accounts.len() > 256 {
        return Err(Status::invalid_argument(
            "expected 1..256 repository accounts",
        ));
    }
    config
        .accounts
        .into_iter()
        .map(|a| {
            let token = kasu_object::read_token(
                &path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(a.token_file),
            )?;
            let restricted = a.read_directories.is_some() || a.read_directories_hex.is_some();
            let mut dirs = a
                .read_directories
                .unwrap_or_default()
                .into_iter()
                .map(|p| kasu_core::RepoPath::new(p.into_bytes()))
                .collect::<kasu_core::Result<Vec<_>>>()
                .map_err(|_| Status::invalid_argument("invalid readable directory"))?;
            for hex in a.read_directories_hex.unwrap_or_default() {
                dirs.push(
                    kasu_core::RepoPath::new(
                        hex::decode(hex).map_err(|_| {
                            Status::invalid_argument("invalid readable directory hex")
                        })?,
                    )
                    .map_err(|_| Status::invalid_argument("invalid readable directory"))?,
                );
            }
            Ok(RepositoryAccount {
                name: a.name,
                token,
                write_policy: a.write_policy,
                read_directories: restricted.then_some(dirs),
            })
        })
        .collect()
}
impl Backend {
    pub fn with_repository_accounts(
        mut self,
        mut accounts: Vec<RepositoryAccount>,
    ) -> Result<Self, Status> {
        if accounts.is_empty() || accounts.len() > 256 {
            return Err(Status::invalid_argument(
                "expected 1..256 repository accounts",
            ));
        }
        for account in &mut accounts {
            if let Some(dirs) = &mut account.read_directories {
                if dirs.is_empty()
                    || dirs.len() > 128
                    || dirs.iter().any(|p| p.as_bytes().len() > 4096)
                {
                    return Err(Status::invalid_argument(
                        "expected 1..128 bounded readable directories",
                    ));
                }
                dirs.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
                dirs.dedup();
                let all = dirs.clone();
                dirs.retain(|p| {
                    !all.iter()
                        .any(|a| a != p && crate::read_scope::below(p.as_bytes(), a.as_bytes()))
                });
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for (i, a) in accounts.iter().enumerate() {
            kasu_object::bearer(&a.token)?;
            if a.name.is_empty()
                || a.name.len() > 128
                || !names.insert(&a.name)
                || self.token.as_deref() == Some(&a.token)
                || self.object_token.as_deref() == Some(&a.token)
                || accounts[..i].iter().any(|b| b.token == a.token)
                || self
                    .projection_accounts
                    .iter()
                    .any(|b| b.token == a.token || b.name == a.name)
            {
                return Err(Status::invalid_argument(
                    "invalid or overlapping repository account configuration",
                ));
            }
            if let Some(p) = &a.write_policy {
                for rules in [
                    &p.allowed_prefixes,
                    &p.force_prefixes,
                    &p.allowed_ref_patterns,
                    &p.force_ref_patterns,
                    &p.allowed_path_prefixes,
                ] {
                    if rules.len() > 128 || rules.iter().any(|r| r.len() > 4096) {
                        return Err(Status::invalid_argument(
                            "repository account policy exceeds limit",
                        ));
                    }
                }
                for path in &p.allowed_path_prefixes {
                    let path = path.strip_suffix(b"/").unwrap_or(path);
                    if !path.is_empty() && kasu_core::RepoPath::new(path).is_err() {
                        return Err(Status::invalid_argument("invalid repository account path"));
                    }
                }
            }
        }
        self.repository_accounts = Arc::new(accounts);
        Ok(self)
    }
    pub(crate) fn writer(&self, authorization: Option<&str>) -> Result<Arc<Self>, Status> {
        self.scope_writer(&self.authenticate(authorization)?)
            .map(Arc::new)
    }
    pub(crate) fn scope_writer(&self, access: &Access<'_>) -> Result<Self, Status> {
        let mut scoped = self.clone();
        scoped.writer_account = match access {
            Access::Repository => None,
            Access::Account(account) if account.write_policy.is_some() => Some(
                self.repository_accounts
                    .iter()
                    .position(|a| std::ptr::eq(a, *account))
                    .ok_or_else(|| Status::internal("account identity missing"))?,
            ),
            _ => {
                return Err(Status::permission_denied(
                    "credentials do not authorize writes",
                ));
            }
        };
        scoped.read_scope = scoped
            .writer_account
            .and_then(|i| self.repository_accounts[i].read_directories.as_ref())
            .map(|dirs| Arc::new(crate::read_scope::ReadScope::new(&self.repository, dirs)));
        Ok(scoped)
    }
    fn account_policy(&self) -> Option<&WritePolicy> {
        self.writer_account
            .and_then(|i| self.repository_accounts[i].write_policy.as_ref())
    }
    pub(crate) fn allows_ref(&self, name: &[u8]) -> bool {
        self.write_policy.allows_ref(name)
            && self.account_policy().is_none_or(|p| p.allows_ref(name))
    }
    pub(crate) fn allows_force(&self, name: &[u8]) -> bool {
        self.write_policy.allows_force(name)
            && self.account_policy().is_none_or(|p| p.allows_force(name))
    }
    pub(crate) fn allows_create(&self) -> bool {
        self.write_policy.allow_create && self.account_policy().is_none_or(|p| p.allow_create)
    }
    pub(crate) fn allows_delete(&self) -> bool {
        self.write_policy.allow_delete && self.account_policy().is_none_or(|p| p.allow_delete)
    }
    pub(crate) fn unrestricted_paths(&self) -> bool {
        self.read_scope.is_none()
            && self.write_policy.unrestricted_paths()
            && self.account_policy().is_none_or(|p| p.unrestricted_paths())
    }
    pub(crate) fn allows_path(&self, path: &[u8]) -> bool {
        self.read_allows(path)
            && self.write_policy.allows_path(path)
            && self.account_policy().is_none_or(|p| p.allows_path(path))
    }
    pub(crate) fn write_actor<'a>(&'a self, fallback: &'a [u8]) -> &'a [u8] {
        self.write_principal()
            .map(str::as_bytes)
            .unwrap_or(fallback)
    }
    pub(crate) fn write_principal(&self) -> Option<&str> {
        self.writer_account
            .map(|i| self.repository_accounts[i].name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn backend(path: &Path) -> Backend {
        let meta = Arc::new(kasu_store::SqliteStore::memory().unwrap());
        Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            Arc::new(kasu_store::FsBlobStore::open(path).unwrap()),
            meta,
            Some("operator".into()),
        )
    }
    fn account(token: &str) -> RepositoryAccount {
        RepositoryAccount {
            read_directories: None,
            name: "writer".into(),
            token: token.into(),
            write_policy: Some(WritePolicy::default()),
        }
    }
    fn projection(token: &str) -> crate::ProjectionAccount {
        crate::ProjectionAccount {
            name: "projection".into(),
            token: token.into(),
            directories: vec![kasu_core::RepoPath::new(b"app".as_slice()).unwrap()],
        }
    }
    #[test]
    fn account_credentials_are_unambiguous_in_each_builder_order() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            backend(dir.path())
                .with_repository_accounts(vec![account("operator")])
                .is_err()
        );
        assert!(
            backend(dir.path())
                .with_object_token("hop".into())
                .unwrap()
                .with_repository_accounts(vec![account("hop")])
                .is_err()
        );
        assert!(
            backend(dir.path())
                .with_repository_accounts(vec![account("hop")])
                .unwrap()
                .with_object_token("hop".into())
                .is_err()
        );
        assert!(
            backend(dir.path())
                .with_projection_accounts(vec![projection("pat")])
                .unwrap()
                .with_repository_accounts(vec![account("pat")])
                .is_err()
        );
        assert!(
            backend(dir.path())
                .with_repository_accounts(vec![account("pat")])
                .unwrap()
                .with_projection_accounts(vec![projection("pat")])
                .is_err()
        );
        assert!(
            backend(dir.path())
                .with_repository_accounts(vec![account("same"), account("same")])
                .is_err()
        );
        let state = backend(dir.path())
            .with_repository_accounts(vec![account("pat")])
            .unwrap();
        assert!(state.authenticate(None).is_err());
        assert!(state.authenticate(Some("Bearer wrong")).is_err());
        assert_eq!(
            state.transport_identity(Some("Bearer pat"), false).unwrap(),
            Some(crate::admission::Identity::Account(0))
        );
        assert_eq!(
            state
                .writer(Some("Basic eDpwYXQ="))
                .unwrap()
                .write_principal(),
            Some("writer")
        );
    }
    #[test]
    fn bounded_file_config_is_strict_relative_and_defaults_to_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.json");
        std::fs::write(dir.path().join("writer.pat"), b"private-pat\n").unwrap();
        std::fs::write(
            &path,
            br#"{"accounts":[{"name":"reader","token_file":"writer.pat"}]}"#,
        )
        .unwrap();
        let state = backend(&dir.path().join("objects"))
            .with_repository_accounts(load_repository_accounts(&path).unwrap())
            .unwrap();
        state.authorize(Some("Bearer private-pat")).unwrap();
        assert!(state.writer(Some("Bearer private-pat")).is_err());
        for bad in [br#"{"accounts":[],"token":"secret-string"}"#.as_slice(), br#"{"accounts":[{"name":"w","token_file":"writer.pat","write_policy":{"typo":true}}]}"#] {
            std::fs::write(&path, bad).unwrap(); let e=load_repository_accounts(&path).err().unwrap();
            assert!(!e.message().contains("secret-string")); assert!(!e.message().contains("private-pat"));
        }
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(load_repository_accounts(&path).is_err());
        let mut a = account("pat");
        a.write_policy.as_mut().unwrap().allowed_path_prefixes = vec![b"../outside".to_vec()];
        assert!(
            backend(&dir.path().join("other"))
                .with_repository_accounts(vec![a])
                .is_err()
        );
    }
}

#[cfg(test)]
mod read_config_tests {
    use super::*;
    #[test]
    fn read_directory_configuration_is_explicit_bounded_and_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.json");
        std::fs::write(dir.path().join("reader.pat"), "reader").unwrap();
        let meta = Arc::new(kasu_store::SqliteStore::memory().unwrap());
        let state = || {
            Backend::new(
                b"repo".to_vec(),
                meta.clone(),
                Arc::new(kasu_store::FsBlobStore::open(dir.path().join("objects")).unwrap()),
                meta.clone(),
                None,
            )
        };
        std::fs::write(&path,br#"{"accounts":[{"name":"reader","token_file":"reader.pat","read_directories":["app/sub","app","app"],"read_directories_hex":["ff"]}]}"#).unwrap();
        let backend = state()
            .with_repository_accounts(load_repository_accounts(&path).unwrap())
            .unwrap();
        let scope = backend.reader(Some("Bearer reader")).unwrap();
        let dirs = &scope.read_scope.as_ref().unwrap().directories;
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].as_bytes(), b"app");
        assert_eq!(dirs[1].as_bytes(), b"\xff");
        assert!(backend.reader(None).is_err());
        for fields in [
            r#""read_directories":[]"#,
            r#""read_directories_hex":[]"#,
            r#""read_directories":["../app"]"#,
            r#""read_directories_hex":["ff0"]"#,
        ] {
            std::fs::write(&path,format!("{{\"accounts\":[{{\"name\":\"reader\",\"token_file\":\"reader.pat\",{fields}}}]}}" )).unwrap();
            assert!(
                load_repository_accounts(&path)
                    .and_then(|accounts| state().with_repository_accounts(accounts))
                    .is_err()
            );
        }
    }
}
