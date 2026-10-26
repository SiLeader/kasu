use kasu_core::*;
use kasu_derived::{Ctx, Engine};
use kasu_ops::Operations;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_sim::faults::{Fault, FaultStore};
use kasu_store::*;
use std::sync::Arc;

struct ReleaseOnDrop(Arc<kasu_sim::faults::CommitGate>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    store: Arc<SqliteStore>,
    faults: Arc<FaultStore>,
    blobs: Arc<FsBlobStore>,
    backend: Arc<Backend>,
    request: p::LandRequest,
    root: ChangesetId,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteStore::open(temp.path().join("meta.db")).unwrap());
        let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
        let faults = Arc::new(FaultStore::new(store.clone()));
        let backend = Arc::new(Backend::new(
            b"repo".to_vec(),
            store.clone(),
            blobs.clone(),
            faults.clone(),
            None,
        ));
        let engine = Engine::standard();
        let ops = Operations {
            engine: &engine,
            ctx: Ctx {
                repo: Repository {
                    meta: store.as_ref(),
                    blobs: blobs.as_ref(),
                },
            },
        };
        let s = Signature {
            name: b"A".as_slice().into(),
            email: b"a@b".as_slice().into(),
            timestamp: 0,
            tz: b"+0000".as_slice().into(),
        };
        let root = ops
            .create_changeset(&Changeset {
                parents: vec![],
                author: s.clone(),
                committer: s.clone(),
                message: b"root".as_slice().into(),
                changes: vec![],
                git_provenance: None,
            })
            .unwrap();
        ops.ctx.repo.associate_changeset(b"repo", root).unwrap();
        let blob = ops.ctx.repo.put_blob(b"content").unwrap();
        ops.ctx.repo.associate_blob(b"repo", blob).unwrap();
        let source = ops
            .create_changeset(&Changeset {
                parents: vec![root],
                author: s.clone(),
                committer: s,
                message: b"source".as_slice().into(),
                changes: vec![FileChange::Set {
                    path: RepoPath::new(b"file".as_slice()).unwrap(),
                    entry: Entry::File {
                        blob,
                        executable: false,
                    },
                }],
                git_provenance: None,
            })
            .unwrap();
        ops.ctx.repo.associate_changeset(b"repo", source).unwrap();
        store
            .compare_and_swap(
                b"repo",
                &[RefUpdate {
                    name: b"refs/heads/main".to_vec(),
                    expected: RefState::default(),
                    new: Some(root),
                }],
                b"setup",
                0,
            )
            .unwrap();
        let request = p::LandRequest {
            required_ancestor: Vec::new(),
            repo: b"repo".to_vec(),
            id: source.0.to_vec(),
            name: b"refs/heads/main".to_vec(),
            idempotency_key: b"key".to_vec(),
            max_attempts: 3,
            timeout_ms: 1000,
        };
        Self {
            _temp: temp,
            store,
            faults,
            blobs,
            backend,
            request,
            root,
        }
    }
    fn assert_unlanded(&self) {
        assert_eq!(
            self.store.resolve(b"repo", b"refs/heads/main").unwrap(),
            RefState {
                target: Some(self.root),
                version: 1
            }
        );
        assert_eq!(self.store.tail(b"repo", 0, 10).unwrap().len(), 1);
        assert_eq!(self.store.land_record(b"repo", b"key").unwrap(), None);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_failure_and_lost_reply_recover_without_duplicate_landing() {
    for fault in [Fault::BeforeCommit, Fault::LostReply] {
        let f = Fixture::new();
        f.faults.inject(b"key".to_vec(), fault);
        let response = f.backend.clone().land(f.request.clone()).await.unwrap();
        assert_eq!(response.outcome, p::LandOutcome::Landed as i32);
        match fault {
            Fault::BeforeCommit => assert_eq!(response.attempts, 2),
            Fault::LostReply => {
                assert!(response.replayed);
                assert_eq!(response.attempts, 1);
            }
        }
        assert_eq!(f.store.tail(b"repo", 0, 10).unwrap().len(), 2);
        let again = f.backend.clone().land(f.request).await.unwrap();
        assert!(again.replayed);
        assert_eq!(again.id, response.id);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_budget_can_resubmit_the_same_key() {
    let f = Fixture::new();
    f.faults.inject(b"key".to_vec(), Fault::BeforeCommit);
    let mut request = f.request.clone();
    request.max_attempts = 1;
    let response = f.backend.clone().land(request).await.unwrap();
    assert_eq!(response.outcome, p::LandOutcome::Retry as i32);
    assert_eq!(response.attempts, 1);
    f.assert_unlanded();
    assert_eq!(
        f.backend.clone().land(f.request).await.unwrap().outcome,
        p::LandOutcome::Landed as i32
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_deadline_and_dropped_future_prevent_a_late_commit() {
    for mode in 0..3 {
        let f = Fixture::new();
        let gate = f.faults.pause_next_commit();
        let _release = ReleaseOnDrop(gate.clone());
        let cancel = LandCancellation::new();
        let c = cancel.clone();
        let state = f.backend.clone();
        let mut request = f.request.clone();
        if mode == 1 {
            request.timeout_ms = 50;
        }
        let task = tokio::spawn(async move { state.land_with_cancellation(request, c).await });
        gate.wait_reached();
        if mode == 0 {
            cancel.cancel();
        } else if mode == 2 {
            task.abort();
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("Land returns before paused storage resumes");
        if mode == 2 {
            assert!(result.unwrap_err().is_cancelled());
        } else {
            assert_eq!(
                result.unwrap().unwrap().outcome,
                p::LandOutcome::Retry as i32
            );
        }
        gate.release();
        gate.wait_finished();
        f.assert_unlanded();
    }
}
#[test]
fn policy_is_rechecked_between_preparation_and_commit() {
    let f = Fixture::new();
    let operation = f.backend.begin_land(f.request.clone()).unwrap();
    let LandPreparation::Ready(prepared) = f.backend.prepare_land(&operation, 1).unwrap() else {
        panic!("expected prepared attempt")
    };
    let denied = Backend::new(
        b"repo".to_vec(),
        f.store.clone(),
        f.blobs.clone(),
        f.store.clone(),
        None,
    )
    .with_write_policy(WritePolicy {
        allowed_prefixes: vec![],
        ..Default::default()
    });
    let LandAttempt::Complete(response) = denied.commit_land(prepared, &|| false).unwrap() else {
        panic!("must deny")
    };
    assert_eq!(response.outcome, p::LandOutcome::Denied as i32);
    assert_eq!(response.attempts, 1);
    f.assert_unlanded();
    let LandPreparation::Ready(prepared) = f.backend.prepare_land(&operation, 1).unwrap() else {
        panic!("expected prepared attempt")
    };
    let denied = Backend::new(
        b"repo".to_vec(),
        f.store.clone(),
        f.blobs.clone(),
        f.store.clone(),
        None,
    )
    .with_write_policy(WritePolicy {
        allowed_path_prefixes: vec![b"other".to_vec()],
        ..Default::default()
    });
    let LandAttempt::Complete(response) = denied.commit_land(prepared, &|| false).unwrap() else {
        panic!("must deny final tree")
    };
    assert_eq!(response.outcome, p::LandOutcome::Denied as i32);
    f.assert_unlanded();
}
