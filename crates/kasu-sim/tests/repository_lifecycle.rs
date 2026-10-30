use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const AUTH: Option<&str> = Some("Bearer operator");
struct Fixture {
    _temp: tempfile::TempDir,
    store: Arc<SqliteStore>,
    backend: Arc<Backend>,
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open(temp.path().join("meta.db")).unwrap());
    let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
    let backend = Arc::new(Backend::with_registry(
        store.clone(),
        blobs,
        store.clone(),
        store.clone(),
        Some("operator".into()),
    ));
    backend
        .create_repository(
            AUTH,
            p::CreateRepositoryRequest {
                name: "team/app".into(),
                ..Default::default()
            },
        )
        .unwrap();
    Fixture {
        _temp: temp,
        store,
        backend,
    }
}
fn signature(n: i64) -> p::Signature {
    p::Signature {
        name: b"A".to_vec(),
        email: b"a@b".to_vec(),
        timestamp: n,
        tz: b"+0000".to_vec(),
    }
}
/// A changeset with no parents and a unique message, ready to land.
fn draft(backend: &Backend, n: i64) -> Vec<u8> {
    backend
        .scope(AUTH, b"team/app", Mode::Write)
        .unwrap()
        .create_changeset(p::CreateChangesetRequest {
            repo: b"team/app".to_vec(),
            parents: vec![],
            changes: vec![],
            metadata: Some(p::CommitMetadata {
                author: Some(signature(n)),
                committer: Some(signature(n)),
                message: format!("draft {n}").into_bytes(),
            }),
        })
        .unwrap()
        .id
}
fn land(id: Vec<u8>, n: usize) -> p::LandRequest {
    p::LandRequest {
        repo: b"team/app".to_vec(),
        id,
        name: format!("refs/heads/b{n}").into_bytes(),
        idempotency_key: format!("land-{n}").into_bytes(),
        ..Default::default()
    }
}
fn key(f: &Fixture) -> kasu_core::RepoKey {
    f.store
        .resolve_repository_name(&kasu_core::RepoName::new("team/app").unwrap(), 0)
        .unwrap()
        .unwrap()
        .key
}

/// A Land prepared before the repository is deleted cannot commit after it:
/// the ref transaction re-reads the repository state.
#[test]
fn a_prepared_land_cannot_commit_into_a_deleted_repository() {
    let f = fixture();
    let key = key(&f);
    let state = f.backend.scope(AUTH, b"team/app", Mode::Write).unwrap();
    let operation = state.begin_land(land(draft(&f.backend, 1), 0)).unwrap();
    let LandPreparation::Ready(prepared) = state.prepare_land(&operation, 1).unwrap() else {
        panic!("Land completed without a commit");
    };
    f.backend
        .delete_repository(
            AUTH,
            p::DeleteRepositoryRequest {
                name: "team/app".into(),
            },
        )
        .unwrap();
    let error = state.commit_land(prepared, &|| false).err().unwrap();
    assert_eq!(error.code(), tonic::Code::NotFound);
    assert!(f.store.tail(key.as_bytes(), 0, 10).unwrap().is_empty());
    assert!(
        f.store
            .land_record(key.as_bytes(), b"land-0")
            .unwrap()
            .is_none()
    );
}

/// Concurrent Lands race a deletion; once the deletion commits, no ref
/// changes, and every reported landing is in the ref log.
#[test]
fn concurrent_lands_never_commit_after_a_deletion() {
    for _ in 0..4 {
        let f = fixture();
        let key = key(&f);
        let workers = 8;
        let drafts: Vec<Vec<_>> = (0..workers)
            .map(|w| {
                (0..16)
                    .map(|n| draft(&f.backend, (w * 100 + n) as i64))
                    .collect()
            })
            .collect();
        let start = Arc::new(Barrier::new(workers + 1));
        let landed = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (committed, first_commit) = std::sync::mpsc::channel();
        let threads: Vec<_> = drafts
            .into_iter()
            .enumerate()
            .map(|(w, drafts)| {
                let backend = f.backend.clone();
                let start = start.clone();
                let landed = landed.clone();
                let stop = stop.clone();
                let committed = committed.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for (n, id) in drafts.into_iter().enumerate() {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        // Resolution may already see the deletion.
                        let Ok(state) = backend.scope(AUTH, b"team/app", Mode::Write) else {
                            break;
                        };
                        let Ok(operation) = state.begin_land(land(id, w * 100 + n)) else {
                            break;
                        };
                        let prepared = match state.prepare_land(&operation, 1) {
                            Ok(LandPreparation::Ready(prepared)) => prepared,
                            Ok(LandPreparation::Complete(_)) => continue,
                            Err(_) => break,
                        };
                        match state.commit_land(prepared, &|| false) {
                            Ok(LandAttempt::Complete(response))
                                if response.outcome == p::LandOutcome::Landed as i32 =>
                            {
                                landed.fetch_add(1, Ordering::SeqCst);
                                if n == 0 {
                                    committed.send(()).unwrap();
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                assert!(
                                    matches!(
                                        error.code(),
                                        tonic::Code::NotFound | tonic::Code::FailedPrecondition
                                    ),
                                    "{error}"
                                );
                                break;
                            }
                        }
                    }
                })
            })
            .collect();
        drop(committed);
        start.wait();
        first_commit
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("no Land committed before deletion");
        f.backend
            .delete_repository(
                AUTH,
                p::DeleteRepositoryRequest {
                    name: "team/app".into(),
                },
            )
            .unwrap();
        let at_deletion = f.store.tail(key.as_bytes(), 0, 10_000).unwrap().len();
        stop.store(true, Ordering::SeqCst);
        for thread in threads {
            thread.join().unwrap();
        }
        let log = f.store.tail(key.as_bytes(), 0, 10_000).unwrap();
        assert!(!log.is_empty(), "no Land raced the deletion");
        assert_eq!(log.len(), at_deletion, "a ref changed after deletion");
        assert_eq!(log.len(), landed.load(Ordering::SeqCst));
        // Purging removes the refs that did land.
        f.backend.purge_deleted_repositories(i64::MAX - 1).unwrap();
        assert!(f.store.list(key.as_bytes(), b"").unwrap().is_empty());
    }
}
