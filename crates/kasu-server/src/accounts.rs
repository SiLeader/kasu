use crate::{Backend, WritePolicy};
use kasu_core::{RepoKey, RepoName, RepoPath};
use kasu_store::RepositoryRecord;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tonic::Status;

/// A repository name glob: `*` matches within one segment, a `**` segment
/// matches one or more segments. `Legacy` names the repository served by
/// pre-registry configuration, by key, so it survives renames.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoPattern {
    Legacy,
    Glob(Vec<String>),
}
impl RepoPattern {
    pub fn parse(pattern: &str) -> Result<Self, Status> {
        let invalid = || Status::invalid_argument("invalid repository pattern");
        if pattern.is_empty() || pattern.len() > kasu_core::MAX_REPO_NAME_BYTES {
            return Err(invalid());
        }
        let segments: Vec<String> = pattern
            .to_ascii_lowercase()
            .split('/')
            .map(str::to_owned)
            .collect();
        for segment in &segments {
            if segment.is_empty()
                || segment.len() > 64
                || !segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-*".contains(&b))
                || (segment.contains("**") && segment != "**")
            {
                return Err(invalid());
            }
        }
        if segments.iter().filter(|s| *s != "**").count() > kasu_core::MAX_REPO_NAME_SEGMENTS {
            return Err(invalid());
        }
        Ok(Self::Glob(segments))
    }
    fn matches_name(segments: &[String], name: &[&str]) -> bool {
        match (segments.first(), name.first()) {
            (None, None) => true,
            (Some(p), _) if p == "**" => {
                (1..=name.len()).any(|n| Self::matches_name(&segments[1..], &name[n..]))
            }
            (Some(p), Some(n)) => {
                crate::write::glob(p.as_bytes(), n.as_bytes())
                    && Self::matches_name(&segments[1..], &name[1..])
            }
            _ => false,
        }
    }
    /// Higher is more specific: exact names, then more literal segments,
    /// then more literal bytes, then fewer multi-segment wildcards.
    fn specificity(&self) -> (bool, usize, usize, std::cmp::Reverse<usize>) {
        match self {
            Self::Legacy => (true, usize::MAX, usize::MAX, std::cmp::Reverse(0)),
            Self::Glob(segments) => (
                !segments.iter().any(|s| s.contains('*')),
                segments.iter().filter(|s| !s.contains('*')).count(),
                segments
                    .iter()
                    .map(|s| s.bytes().filter(|b| *b != b'*').count())
                    .sum(),
                std::cmp::Reverse(segments.iter().filter(|s| *s == "**").count()),
            ),
        }
    }
    /// Match a resolved repository: by key for Legacy, otherwise against its
    /// current and former primary names (never the alias a request used).
    pub(crate) fn matches(&self, record: &RepositoryRecord, legacy: Option<&RepoKey>) -> bool {
        match self {
            Self::Legacy => legacy == Some(&record.key),
            Self::Glob(segments) => record.names().any(|name| {
                let normalized = name.normalized();
                let parts: Vec<&str> = normalized.split('/').collect();
                Self::matches_name(segments, &parts)
            }),
        }
    }
    pub fn matches_name_for_test(&self, name: &RepoName) -> bool {
        let record = RepositoryRecord::new(RepoKey::legacy(name), name.clone(), b"", 0);
        self.matches(&record, None)
    }
}
/// Pick the most specific matching pattern among `candidates`; earlier
/// entries win ties. Permissions are never unioned across entries.
pub(crate) fn most_specific<'a, T>(
    candidates: impl IntoIterator<Item = (&'a RepoPattern, T)>,
    record: &RepositoryRecord,
    legacy: Option<&RepoKey>,
) -> Option<T> {
    let mut best: Option<(&RepoPattern, T)> = None;
    for (pattern, value) in candidates {
        if pattern.matches(record, legacy)
            && best
                .as_ref()
                .is_none_or(|(b, _)| pattern.specificity() > b.specificity())
        {
            best = Some((pattern, value));
        }
    }
    best.map(|(_, value)| value)
}

/// Permissions of one account on the repositories its patterns match.
pub struct RepositoryGrant {
    pub repositories: Vec<RepoPattern>,
    /// None is read-only.
    pub write_policy: Option<WritePolicy>,
    /// Some restricts native reads to these directories.
    pub read_directories: Option<Vec<RepoPath>>,
}
// A credential is deliberately not Debug. None grants repository reads only.
pub struct RepositoryAccount {
    pub name: String,
    pub token: String,
    /// Pre-grant form: a single grant on the legacy repository, used when
    /// `grants` is empty.
    pub write_policy: Option<WritePolicy>,
    pub read_directories: Option<Vec<RepoPath>>,
    pub grants: Vec<RepositoryGrant>,
}
impl RepositoryAccount {
    pub(crate) fn grant(&self, index: usize) -> &RepositoryGrant {
        &self.grants[index]
    }
    pub(crate) fn grant_for(
        &self,
        record: &RepositoryRecord,
        legacy: Option<&RepoKey>,
    ) -> Option<usize> {
        most_specific(
            self.grants.iter().enumerate().flat_map(|(index, grant)| {
                grant
                    .repositories
                    .iter()
                    .map(move |pattern| (pattern, index))
            }),
            record,
            legacy,
        )
    }
    pub(crate) fn restricted_somewhere(&self) -> bool {
        self.grants.iter().any(|g| g.read_directories.is_some())
    }
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
    #[serde(default)]
    grants: Option<Vec<GrantFile>>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantFile {
    repositories: Vec<String>,
    #[serde(default = "yes")]
    read: bool,
    #[serde(default)]
    write_policy: Option<WritePolicy>,
    #[serde(default)]
    read_directories: Option<Vec<String>>,
    #[serde(default)]
    read_directories_hex: Option<Vec<String>>,
}
fn yes() -> bool {
    true
}
fn directories(
    text: Option<Vec<String>>,
    hex: Option<Vec<String>>,
) -> Result<Option<Vec<RepoPath>>, Status> {
    let restricted = text.is_some() || hex.is_some();
    let mut dirs = text
        .unwrap_or_default()
        .into_iter()
        .map(|p| RepoPath::new(p.into_bytes()))
        .collect::<kasu_core::Result<Vec<_>>>()
        .map_err(|_| Status::invalid_argument("invalid readable directory"))?;
    for hex in hex.unwrap_or_default() {
        dirs.push(
            RepoPath::new(
                hex::decode(hex)
                    .map_err(|_| Status::invalid_argument("invalid readable directory hex"))?,
            )
            .map_err(|_| Status::invalid_argument("invalid readable directory"))?,
        );
    }
    Ok(restricted.then_some(dirs))
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
            let read_directories = directories(a.read_directories, a.read_directories_hex)?;
            let grants = match a.grants {
                None => Vec::new(),
                Some(grants) => {
                    // Mixing both forms would make the legacy grant ambiguous.
                    if a.write_policy.is_some() || read_directories.is_some() || grants.is_empty() {
                        return Err(Status::invalid_argument(
                            "grants replace account-level write_policy and read_directories",
                        ));
                    }
                    grants
                        .into_iter()
                        .map(|g| {
                            if !g.read {
                                return Err(Status::invalid_argument("grants must allow reads"));
                            }
                            Ok(RepositoryGrant {
                                repositories: g
                                    .repositories
                                    .iter()
                                    .map(|p| RepoPattern::parse(p))
                                    .collect::<Result<_, _>>()?,
                                write_policy: g.write_policy,
                                read_directories: directories(
                                    g.read_directories,
                                    g.read_directories_hex,
                                )?,
                            })
                        })
                        .collect::<Result<_, Status>>()?
                }
            };
            Ok(RepositoryAccount {
                name: a.name,
                token,
                write_policy: a.write_policy,
                read_directories,
                grants,
            })
        })
        .collect()
}
fn normalize_directories(dirs: &mut Vec<RepoPath>) -> Result<(), Status> {
    if dirs.is_empty() || dirs.len() > 128 || dirs.iter().any(|p| p.as_bytes().len() > 4096) {
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
    Ok(())
}
pub(crate) fn validate_policy(p: &WritePolicy) -> Result<(), Status> {
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
    Ok(())
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
            if account.grants.is_empty() {
                account.grants.push(RepositoryGrant {
                    repositories: vec![RepoPattern::Legacy],
                    write_policy: account.write_policy.take(),
                    read_directories: account.read_directories.take(),
                });
            } else if account.write_policy.is_some() || account.read_directories.is_some() {
                return Err(Status::invalid_argument(
                    "grants replace account-level write_policy and read_directories",
                ));
            }
            if account.grants.len() > 128
                || account
                    .grants
                    .iter()
                    .any(|g| g.repositories.is_empty() || g.repositories.len() > 128)
            {
                return Err(Status::invalid_argument("expected 1..128 bounded grants"));
            }
            for grant in &mut account.grants {
                if let Some(dirs) = &mut grant.read_directories {
                    normalize_directories(dirs)?;
                }
                if let Some(policy) = &grant.write_policy {
                    validate_policy(policy)?;
                }
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
        }
        self.repository_accounts = Arc::new(accounts);
        Ok(self)
    }
    fn account_policy(&self) -> Option<&WritePolicy> {
        self.writer_grant
            .and_then(|(a, g)| self.repository_accounts[a].grant(g).write_policy.as_ref())
    }
    fn repository_policy(&self) -> Option<&WritePolicy> {
        self.repo
            .as_ref()
            .and_then(|r| r.repo.config.write_policy.as_ref())
    }
    /// Server ∧ repository ∧ account: narrower settings never widen broader ones.
    fn policies(&self) -> impl Iterator<Item = &WritePolicy> {
        std::iter::once(&self.write_policy)
            .chain(self.repository_policy())
            .chain(self.account_policy())
    }
    pub(crate) fn allows_ref(&self, name: &[u8]) -> bool {
        self.policies().all(|p| p.allows_ref(name))
    }
    pub(crate) fn allows_force(&self, name: &[u8]) -> bool {
        self.policies().all(|p| p.allows_force(name))
    }
    pub(crate) fn allows_create(&self) -> bool {
        self.policies().all(|p| p.allow_create)
    }
    pub(crate) fn allows_delete(&self) -> bool {
        self.policies().all(|p| p.allow_delete)
    }
    pub(crate) fn unrestricted_paths(&self) -> bool {
        self.read_scope.is_none() && self.policies().all(|p| p.unrestricted_paths())
    }
    pub(crate) fn allows_path(&self, path: &[u8]) -> bool {
        self.read_allows(path) && self.policies().all(|p| p.allows_path(path))
    }
    pub(crate) fn write_actor<'a>(&'a self, fallback: &'a [u8]) -> &'a [u8] {
        self.write_principal()
            .map(str::as_bytes)
            .unwrap_or(fallback)
    }
    pub(crate) fn write_principal(&self) -> Option<&str> {
        self.writer_grant
            .map(|(a, _)| self.repository_accounts[a].name.as_str())
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
            grants: vec![],
        }
    }
    fn projection(token: &str) -> crate::ProjectionAccount {
        crate::ProjectionAccount {
            name: "projection".into(),
            token: token.into(),
            directories: vec![kasu_core::RepoPath::new(b"app".as_slice()).unwrap()],
            scopes: vec![],
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
                .scope(Some("Basic eDpwYXQ="), b"repo", crate::Mode::Write)
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
        assert!(
            state
                .scope(Some("Bearer private-pat"), b"repo", crate::Mode::Write)
                .is_err()
        );
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
    #[test]
    fn repository_patterns_match_segments_case_insensitively_and_rank_specificity() {
        let name = |s: &str| RepoName::new(s).unwrap();
        let glob = |s: &str| RepoPattern::parse(s).unwrap();
        assert!(glob("team-a/*").matches_name_for_test(&name("Team-A/infra")));
        assert!(!glob("team-a/*").matches_name_for_test(&name("team-a/x/y")));
        assert!(glob("team-a/**").matches_name_for_test(&name("team-a/x/y")));
        assert!(!glob("team-a/**").matches_name_for_test(&name("team-a")));
        assert!(glob("**/charts").matches_name_for_test(&name("a/b/charts")));
        assert!(glob("team-*/app").matches_name_for_test(&name("team-b/app")));
        assert!(!glob("team-*/app").matches_name_for_test(&name("other/app")));
        for invalid in ["", "a//b", "a/**x", "a b", "a/b/c/d/e"] {
            assert!(RepoPattern::parse(invalid).is_err(), "{invalid}");
        }
        let record =
            RepositoryRecord::new(RepoKey::generate().unwrap(), name("team-a/infra"), b"", 0);
        let patterns = [
            glob("**"),
            glob("team-a/**"),
            glob("team-a/*"),
            glob("team-a/infra"),
        ];
        let ranked = most_specific(patterns.iter().zip(0..), &record, None);
        assert_eq!(ranked, Some(3));
        let ranked = most_specific(patterns[..3].iter().zip(0..), &record, None);
        assert_eq!(ranked, Some(2));
        // Equal specificity keeps the first entry; permissions are never unioned.
        let ties = [glob("team-a/*"), glob("*/infra")];
        assert_eq!(most_specific(ties.iter().zip(0..), &record, None), Some(0));
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
        let scope = backend
            .scope(Some("Bearer reader"), b"repo", crate::Mode::Read)
            .unwrap();
        let dirs = &scope.read_scope.as_ref().unwrap().directories;
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].as_bytes(), b"app");
        assert_eq!(dirs[1].as_bytes(), b"\xff");
        assert!(backend.scope(None, b"repo", crate::Mode::Read).is_err());
        for fields in [
            r#""read_directories":[]"#,
            r#""read_directories_hex":[]"#,
            r#""read_directories":["../app"]"#,
            r#""read_directories_hex":["ff0"]"#,
            r#""grants":[]"#,
            r#""grants":[{"repositories":["a"],"read":false}]"#,
            r#""write_policy":{},"grants":[{"repositories":["a"]}]"#,
            r#""grants":[{"repositories":["a//b"]}]"#,
        ] {
            std::fs::write(&path,format!("{{\"accounts\":[{{\"name\":\"reader\",\"token_file\":\"reader.pat\",{fields}}}]}}" )).unwrap();
            assert!(
                load_repository_accounts(&path)
                    .and_then(|accounts| state().with_repository_accounts(accounts))
                    .is_err(),
                "{fields}"
            );
        }
    }
}
