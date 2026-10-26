use kasu_core::*;
use kasu_store::*;
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

fn commit(n: u8) -> LandCommit {
    LandCommit {
        name: b"refs/heads/main".to_vec(),
        expected: RefState::default(),
        new: Some(ChangesetId([n; 32])),
        record: LandRecord {
            fingerprint: ObjectId::hash(b"same request"),
            response: vec![n],
        },
    }
}
#[test]
fn results_ref_and_log_are_atomic_durable_and_request_bound() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("meta.db");
    let store = SqliteStore::open(&db).unwrap();
    let c = commit(1);
    assert!(matches!(
        store
            .commit_land(b"repo", b"key", &c, b"actor", 1, &|| false)
            .unwrap(),
        LandCommitResult::Recorded {
            replayed: false,
            ..
        }
    ));
    drop(store);
    let store = SqliteStore::open(&db).unwrap();
    let original = store.land_record(b"repo", b"key").unwrap().unwrap();
    assert_eq!(original, c.record);
    assert_eq!(
        store
            .commit_land(b"repo", b"key", &commit(2), b"actor", 2, &|| true)
            .unwrap(),
        LandCommitResult::Recorded {
            record: original,
            replayed: true
        }
    );
    assert_eq!(
        store.resolve(b"repo", &c.name).unwrap(),
        RefState {
            target: c.new,
            version: 1
        }
    );
    assert_eq!(store.tail(b"repo", 0, 10).unwrap().len(), 1);
    let mut different = commit(2);
    different.record.fingerprint = ObjectId::hash(b"different request");
    assert!(
        store
            .commit_land(b"repo", b"key", &different, b"actor", 2, &|| false)
            .is_err()
    );
    assert!(matches!(
        store
            .commit_land(b"repo", b"other-key", &c, b"actor", 2, &|| false)
            .unwrap(),
        LandCommitResult::Mismatch(_)
    ));
    assert_eq!(store.land_record(b"repo", b"other-key").unwrap(), None);
    assert!(matches!(
        store
            .commit_land(b"other-repo", b"key", &c, b"actor", 2, &|| false)
            .unwrap(),
        LandCommitResult::Recorded {
            replayed: false,
            ..
        }
    ));
    let calls = AtomicUsize::new(0);
    // Cancellation after the SQL writes must roll back all three tables.
    assert_eq!(
        store
            .commit_land(b"cancel", b"key", &c, b"actor", 3, &|| calls
                .fetch_add(1, Ordering::SeqCst)
                > 0)
            .unwrap(),
        LandCommitResult::Cancelled
    );
    assert_eq!(
        store.resolve(b"cancel", &c.name).unwrap(),
        RefState::default()
    );
    assert!(store.tail(b"cancel", 0, 10).unwrap().is_empty());
    assert_eq!(store.land_record(b"cancel", b"key").unwrap(), None);
}

#[test]
fn independent_connections_record_one_result_and_one_landing_for_duplicate_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("meta.db");
    SqliteStore::open(&db).unwrap();
    let barrier = Arc::new(Barrier::new(16));
    let threads: Vec<_> = (1..=16)
        .map(|n| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = SqliteStore::open(db).unwrap();
                barrier.wait();
                store
                    .commit_land(b"repo", b"key", &commit(n), b"actor", 1, &|| false)
                    .unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(
                o,
                LandCommitResult::Recorded {
                    replayed: false,
                    ..
                }
            ))
            .count(),
        1
    );
    let store = SqliteStore::open(db).unwrap();
    let record = store.land_record(b"repo", b"key").unwrap().unwrap();
    assert!(
        outcomes
            .into_iter()
            .all(|outcome| matches!(outcome,LandCommitResult::Recorded {record:r,..} if r==record))
    );
    assert_eq!(
        store.resolve(b"repo", b"refs/heads/main").unwrap().target,
        Some(ChangesetId([record.response[0]; 32]))
    );
    assert_eq!(store.tail(b"repo", 0, 100).unwrap().len(), 1);
}
