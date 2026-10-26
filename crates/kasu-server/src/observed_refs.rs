use crate::admission::ServerStats;
use kasu_core::*;
use kasu_store::*;
use std::sync::{Arc, atomic::Ordering};
pub(crate) struct ObservedRefs {
    pub inner: Arc<dyn GitRefStore>,
    pub stats: Arc<ServerStats>,
}
impl ObservedRefs {
    fn count(&self, op: usize, result: usize) {
        self.stats.cas[op][result].fetch_add(1, Ordering::Relaxed);
    }
}
impl RefStore for ObservedRefs {
    fn capabilities(&self) -> RefCapabilities {
        self.inner.capabilities()
    }
    fn resolve(&self, repo: &[u8], name: &[u8]) -> Result<RefState> {
        self.inner.resolve(repo, name)
    }
    fn list(&self, repo: &[u8], prefix: &[u8]) -> Result<Vec<(Vec<u8>, RefState)>> {
        self.inner.list(repo, prefix)
    }
    fn tail(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<RefLog>> {
        self.inner.tail(repo, after, limit)
    }
    fn land_record(&self, repo: &[u8], key: &[u8]) -> Result<Option<LandRecord>> {
        self.inner.land_record(repo, key)
    }
    fn compare_and_swap(
        &self,
        repo: &[u8],
        updates: &[RefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<CasResult> {
        let result = self.inner.compare_and_swap(repo, updates, actor, time);
        self.count(
            0,
            match &result {
                Ok(CasResult::Applied(_)) => 0,
                Ok(CasResult::Mismatch { .. }) => 1,
                Err(_) => 4,
            },
        );
        result
    }
    fn commit_land(
        &self,
        repo: &[u8],
        key: &[u8],
        commit: &LandCommit,
        actor: &[u8],
        time: i64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<LandCommitResult> {
        let result = self
            .inner
            .commit_land(repo, key, commit, actor, time, cancelled);
        self.count(
            3,
            match &result {
                Ok(LandCommitResult::Recorded {
                    replayed: false, ..
                }) => 0,
                Ok(LandCommitResult::Recorded { replayed: true, .. }) => 2,
                Ok(LandCommitResult::Mismatch(_)) => 1,
                Ok(LandCommitResult::Cancelled) => 3,
                Err(_) => 4,
            },
        );
        result
    }
}
impl GitRefStore for ObservedRefs {
    fn supports_atomic_publication(&self) -> bool {
        self.inner.supports_atomic_publication()
    }
    fn resolve_git(&self, repo: &[u8], name: &[u8]) -> Result<GitRefState> {
        self.inner.resolve_git(repo, name)
    }
    fn list_git(&self, repo: &[u8], prefix: &[u8]) -> Result<Vec<(Vec<u8>, GitRefState)>> {
        self.inner.list_git(repo, prefix)
    }
    fn tail_git(&self, repo: &[u8], after: u64, limit: usize) -> Result<Vec<GitRefLog>> {
        self.inner.tail_git(repo, after, limit)
    }
    fn compare_git(
        &self,
        repo: &[u8],
        updates: &[GitRefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<GitCasResult> {
        let result = self.inner.compare_git(repo, updates, actor, time);
        self.count(
            1,
            match &result {
                Ok(GitCasResult::Applied(_)) => 0,
                Ok(GitCasResult::Mismatch { .. }) => 1,
                Err(_) => 4,
            },
        );
        result
    }
    fn compare_native_and_git(
        &self,
        repo: &[u8],
        native: &[RefUpdate],
        git: &[GitRefUpdate],
        actor: &[u8],
        time: i64,
    ) -> Result<RefPublicationResult> {
        let result = self
            .inner
            .compare_native_and_git(repo, native, git, actor, time);
        self.count(
            2,
            match &result {
                Ok(RefPublicationResult::Applied { .. }) => 0,
                Ok(_) => 1,
                Err(_) => 4,
            },
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metrics_distinguish_conflict_replay_cancellation_and_storage_error() {
        let store = Arc::new(SqliteStore::memory().unwrap());
        let stats = Arc::new(ServerStats::default());
        let refs = ObservedRefs {
            inner: store.clone(),
            stats: stats.clone(),
        };
        assert_eq!(
            refs.capabilities().atomic_batch,
            store.capabilities().atomic_batch
        );
        assert_eq!(
            refs.supports_atomic_publication(),
            store.supports_atomic_publication()
        );
        let native = RefUpdate {
            name: b"refs/heads/native".to_vec(),
            expected: RefState::default(),
            new: Some(ChangesetId([1; 32])),
        };
        assert!(matches!(
            refs.compare_and_swap(b"repo", std::slice::from_ref(&native), b"actor", 0)
                .unwrap(),
            CasResult::Applied(_)
        ));
        assert!(matches!(
            refs.compare_and_swap(b"repo", &[native], b"actor", 0)
                .unwrap(),
            CasResult::Mismatch { .. }
        ));
        let git = GitRefUpdate {
            name: b"refs/tags/git".to_vec(),
            expected: GitRefState::default(),
            new: Some(GitRef::Direct {
                target: GitOid([2; 20]),
                peeled: None,
                kind: GitKind::Commit,
            }),
        };
        assert!(matches!(
            refs.compare_git(b"repo", std::slice::from_ref(&git), b"actor", 0)
                .unwrap(),
            GitCasResult::Applied(_)
        ));
        assert!(matches!(
            refs.compare_git(b"repo", &[git], b"actor", 0).unwrap(),
            GitCasResult::Mismatch { .. }
        ));
        let native = RefUpdate {
            name: b"refs/heads/mixed".to_vec(),
            expected: RefState::default(),
            new: Some(ChangesetId([3; 32])),
        };
        let git = GitRefUpdate {
            name: b"refs/tags/mixed".to_vec(),
            expected: GitRefState::default(),
            new: Some(GitRef::Direct {
                target: GitOid([4; 20]),
                peeled: None,
                kind: GitKind::Commit,
            }),
        };
        assert!(matches!(
            refs.compare_native_and_git(
                b"repo",
                std::slice::from_ref(&native),
                std::slice::from_ref(&git),
                b"actor",
                0
            )
            .unwrap(),
            RefPublicationResult::Applied { .. }
        ));
        assert!(!matches!(
            refs.compare_native_and_git(b"repo", &[native], &[git], b"actor", 0)
                .unwrap(),
            RefPublicationResult::Applied { .. }
        ));
        let mut land = LandCommit {
            name: b"refs/heads/land".to_vec(),
            expected: RefState::default(),
            new: Some(ChangesetId([5; 32])),
            record: LandRecord {
                fingerprint: ObjectId::hash(b"request"),
                response: b"response".to_vec(),
            },
        };
        assert!(matches!(
            refs.commit_land(b"repo", b"key", &land, b"actor", 0, &|| false)
                .unwrap(),
            LandCommitResult::Recorded {
                replayed: false,
                ..
            }
        ));
        assert!(matches!(
            refs.commit_land(b"repo", b"key", &land, b"actor", 0, &|| false)
                .unwrap(),
            LandCommitResult::Recorded { replayed: true, .. }
        ));
        assert!(matches!(
            refs.commit_land(b"repo", b"stale", &land, b"actor", 0, &|| false)
                .unwrap(),
            LandCommitResult::Mismatch(_)
        ));
        land.name = b"refs/heads/cancel".to_vec();
        assert_eq!(
            refs.commit_land(b"repo", b"cancel", &land, b"actor", 0, &|| true)
                .unwrap(),
            LandCommitResult::Cancelled
        );
        land.record.fingerprint = ObjectId::hash(b"different");
        assert!(
            refs.commit_land(b"repo", b"key", &land, b"actor", 0, &|| false)
                .is_err()
        );
        for op in 0..3 {
            assert_eq!(stats.cas[op][0].load(Ordering::Relaxed), 1);
            assert_eq!(stats.cas[op][1].load(Ordering::Relaxed), 1);
        }
        for count in &stats.cas[3] {
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }
        assert_eq!(refs.tail(b"repo", 0, 100).unwrap().len(), 3);
        assert_eq!(refs.tail_git(b"repo", 0, 100).unwrap().len(), 2);
        assert_eq!(
            refs.resolve(b"repo", b"refs/heads/cancel").unwrap(),
            RefState::default()
        );
    }
}
