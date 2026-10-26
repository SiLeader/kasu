use kasu_core::{ChangesetId, GitKind, GitOid, GitRef};
use kasu_store::*;
use std::sync::{Arc, Barrier};

fn updates(n: u8) -> (RefUpdate, GitRefUpdate) {
    (
        RefUpdate {
            name: b"refs/heads/main".to_vec(),
            expected: RefState::default(),
            new: Some(ChangesetId([n; 32])),
        },
        GitRefUpdate {
            name: b"refs/tags/v1".to_vec(),
            expected: GitRefState::default(),
            new: Some(GitRef::Direct {
                target: GitOid([n; 20]),
                kind: GitKind::Commit,
                peeled: None,
            }),
        },
    )
}

#[test]
fn late_sql_failure_rolls_back_both_ref_models_and_both_logs() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("meta.db");
    let store = SqliteStore::open(&db).unwrap();
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_git_log BEFORE INSERT ON git_ref_log BEGIN SELECT RAISE(ABORT, 'injected late write failure'); END;").unwrap();
    let (native, git) = updates(1);
    assert!(store.supports_atomic_publication());
    assert!(
        store
            .compare_native_and_git(
                b"repo",
                std::slice::from_ref(&native),
                std::slice::from_ref(&git),
                b"push",
                1
            )
            .is_err()
    );
    let reopened = SqliteStore::open(&db).unwrap();
    assert_eq!(
        reopened.resolve(b"repo", &native.name).unwrap(),
        RefState::default()
    );
    assert_eq!(
        reopened.resolve_git(b"repo", &git.name).unwrap(),
        GitRefState::default()
    );
    assert!(reopened.tail(b"repo", 0, 100).unwrap().is_empty());
    assert!(reopened.tail_git(b"repo", 0, 100).unwrap().is_empty());
    connection
        .execute_batch("DROP TRIGGER reject_git_log;")
        .unwrap();
    assert!(matches!(
        store
            .compare_native_and_git(b"repo", &[native], &[git], b"push", 1)
            .unwrap(),
        RefPublicationResult::Applied { .. }
    ));
    assert_eq!(reopened.tail(b"repo", 0, 100).unwrap().len(), 1);
    assert_eq!(reopened.tail_git(b"repo", 0, 100).unwrap().len(), 1);
}

#[test]
fn independent_mixed_transactions_have_one_winner_and_matching_logs() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("meta.db");
    SqliteStore::open(&db).unwrap();
    let barrier = Arc::new(Barrier::new(16));
    let workers: Vec<_> = (1..=16)
        .map(|n| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = SqliteStore::open(db).unwrap();
                let (native, git) = updates(n);
                barrier.wait();
                (
                    n,
                    store
                        .compare_native_and_git(b"repo", &[native], &[git], &[n], 1)
                        .unwrap(),
                )
            })
        })
        .collect();
    let winners: Vec<_> = workers
        .into_iter()
        .filter_map(|worker| {
            let (n, result) = worker.join().unwrap();
            matches!(result, RefPublicationResult::Applied { .. }).then_some(n)
        })
        .collect();
    assert_eq!(winners.len(), 1);
    let n = winners[0];
    let store = SqliteStore::open(db).unwrap();
    let (native, git) = updates(n);
    assert_eq!(
        store.resolve(b"repo", &native.name).unwrap(),
        RefState {
            target: native.new,
            version: 1
        }
    );
    assert_eq!(
        store.resolve_git(b"repo", &git.name).unwrap(),
        GitRefState {
            value: git.new,
            version: 1
        }
    );
    let log = store.tail(b"repo", 0, 100).unwrap();
    let git_log = store.tail_git(b"repo", 0, 100).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(git_log.len(), 1);
    assert_eq!(log[0].actor, vec![n]);
    assert_eq!(git_log[0].actor, vec![n]);
    assert_eq!(log[0].new, Some(ChangesetId([n; 32])));
    assert_eq!(
        git_log[0].new,
        store.resolve_git(b"repo", b"refs/tags/v1").unwrap().value
    );
    assert!(store.tail(b"other-repo", 0, 100).unwrap().is_empty());
    assert!(store.tail_git(b"other-repo", 0, 100).unwrap().is_empty());
}
