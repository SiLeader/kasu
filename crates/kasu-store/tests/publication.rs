use kasu_core::{ChangesetId, GitKind, GitOid, GitRef, ObjectId};
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

fn ref_namespaces_are_transactional(store: &dyn DatabaseStore, repo: &[u8]) {
    let (parent, tag) = updates(1);
    let child = RefUpdate {
        name: b"refs/heads/main/child".to_vec(),
        ..parent.clone()
    };
    store
        .compare_and_swap(repo, std::slice::from_ref(&parent), b"test", 0)
        .unwrap();
    assert!(
        store
            .compare_and_swap(repo, std::slice::from_ref(&child), b"test", 0)
            .is_err()
    );
    assert_eq!(
        store.resolve(repo, &child.name).unwrap(),
        RefState::default()
    );
    assert_eq!(store.tail(repo, 0, 100).unwrap().len(), 1);

    let land = LandCommit {
        name: child.name.clone(),
        expected: child.expected,
        new: child.new,
        record: LandRecord {
            fingerprint: ObjectId::hash(b"conflicting land"),
            response: vec![1],
        },
    };
    assert!(
        store
            .commit_land(repo, b"land", &land, b"test", 0, &|| false)
            .is_err()
    );
    assert!(store.land_record(repo, b"land").unwrap().is_none());
    let review = ReviewWrite {
        review: ReviewRow {
            number: 0,
            version: 1,
            state: REVIEW_SUBMITTED,
            target: child.name.clone(),
            head: vec![1; 32],
            author: "test".into(),
            data: vec![],
        },
        expected_version: 0,
        revision: None,
        votes: vec![],
        comments: vec![],
        event: b"submit".to_vec(),
        request: Some(ReviewRequestRecord {
            account: "test".into(),
            key: b"submit".to_vec(),
            fingerprint: vec![1],
            response: vec![2],
        }),
        ref_update: Some(ReviewRefUpdate {
            update: child.clone(),
            actor: b"test".to_vec(),
        }),
        time: 0,
    };
    assert!(store.write_review(repo, &review).is_err());
    assert!(store.review(repo, 1).unwrap().is_none());
    assert!(
        store
            .review_request(repo, "test", b"submit")
            .unwrap()
            .is_none()
    );
    assert!(store.review_log(repo, 0, 100).unwrap().is_empty());

    store
        .compare_git(repo, std::slice::from_ref(&tag), b"test", 0)
        .unwrap();
    let tag_child = GitRefUpdate {
        name: b"refs/tags/v1/child".to_vec(),
        ..tag.clone()
    };
    assert!(
        store
            .compare_git(repo, std::slice::from_ref(&tag_child), b"test", 0)
            .is_err()
    );
    let sibling = RefUpdate {
        name: b"refs/heads/main-other".to_vec(),
        ..parent.clone()
    };
    assert!(
        store
            .compare_native_and_git(
                repo,
                std::slice::from_ref(&sibling),
                &[tag_child],
                b"test",
                0
            )
            .is_err()
    );
    assert_eq!(
        store.resolve(repo, &sibling.name).unwrap(),
        RefState::default()
    );
    assert_eq!(store.tail_git(repo, 0, 100).unwrap().len(), 1);

    // Validate the final batch, not its order: a deletion can free a prefix.
    let deleted = RefUpdate {
        expected: store.resolve(repo, &parent.name).unwrap(),
        new: None,
        ..parent.clone()
    };
    assert!(matches!(
        store
            .compare_and_swap(repo, &[child.clone(), deleted], b"test", 0)
            .unwrap(),
        CasResult::Applied(_)
    ));
    let restored = RefUpdate {
        expected: store.resolve(repo, &parent.name).unwrap(),
        ..parent
    };
    assert!(
        store
            .compare_and_swap(repo, std::slice::from_ref(&restored), b"test", 0)
            .is_err()
    );
    let deleted_child = RefUpdate {
        expected: store.resolve(repo, &child.name).unwrap(),
        new: None,
        ..child
    };
    assert!(matches!(
        store
            .compare_and_swap(repo, &[restored, deleted_child, sibling], b"test", 0)
            .unwrap(),
        CasResult::Applied(_)
    ));
    let tag_parent = GitRefUpdate {
        name: b"refs/tags".to_vec(),
        ..tag
    };
    assert!(store.compare_git(repo, &[tag_parent], b"test", 0).is_err());

    let (mut native, mut git) = updates(2);
    native.name = b"refs/mixed/child".to_vec();
    git.name = b"refs/mixed".to_vec();
    assert!(
        store
            .compare_native_and_git(repo, &[native], &[git], b"test", 0)
            .is_err()
    );
    assert_eq!(
        store.resolve(repo, b"refs/mixed/child").unwrap(),
        RefState::default()
    );
    assert_eq!(
        store.resolve_git(repo, b"refs/mixed").unwrap(),
        GitRefState::default()
    );
    assert_eq!(store.tail(repo, 0, 100).unwrap().len(), 6);
    assert_eq!(store.tail_git(repo, 0, 100).unwrap().len(), 1);

    // Long names can share an indexed prefix without sharing a namespace.
    let prefix = format!("refs/heads/{}", "a".repeat(120));
    let long_child = RefUpdate {
        name: format!("{prefix}/child").into_bytes(),
        ..updates(3).0
    };
    let long_parent = RefUpdate {
        name: prefix.as_bytes().to_vec(),
        ..updates(3).0
    };
    let long_sibling = RefUpdate {
        name: format!("{prefix}-other").into_bytes(),
        ..updates(3).0
    };
    store
        .compare_and_swap(repo, &[long_child, long_sibling], b"test", 0)
        .unwrap();
    assert!(
        store
            .compare_and_swap(repo, &[long_parent], b"test", 0)
            .is_err()
    );
}

fn ref_namespace_race(open: impl Fn() -> Box<dyn GitRefStore> + Sync, repo: &[u8]) {
    for (parent_is_git, child_is_git) in
        [(false, false), (true, true), (false, true), (true, false)]
    {
        let parent = format!("refs/race-{parent_is_git}-{child_is_git}");
        let store = open();
        let mut seed = RefUpdate {
            name: format!("{parent}/seed").into_bytes(),
            ..updates(1).0
        };
        store
            .compare_and_swap(repo, std::slice::from_ref(&seed), b"setup", 0)
            .unwrap();
        seed.expected = store.resolve(repo, &seed.name).unwrap();
        seed.new = None;
        store.compare_and_swap(repo, &[seed], b"setup", 0).unwrap();
        race_ref_pair(
            &open,
            repo,
            [
                (parent.as_bytes().to_vec(), parent_is_git),
                (format!("{parent}/sub").into_bytes(), child_is_git),
            ],
        );
    }
}

fn race_ref_pair(
    open: &(impl Fn() -> Box<dyn GitRefStore> + Sync),
    repo: &[u8],
    refs: [(Vec<u8>, bool); 2],
) {
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let workers: Vec<_> = refs
            .into_iter()
            .map(|(name, git)| {
                let barrier = &barrier;
                scope.spawn(move || {
                    let store = open();
                    barrier.wait();
                    let result = if git {
                        store
                            .compare_git(
                                repo,
                                &[GitRefUpdate {
                                    name,
                                    ..updates(1).1
                                }],
                                b"race",
                                0,
                            )
                            .map(|result| matches!(result, GitCasResult::Applied(_)))
                    } else {
                        store
                            .compare_and_swap(
                                repo,
                                &[RefUpdate {
                                    name,
                                    expected: RefState::default(),
                                    new: Some(ChangesetId([1; 32])),
                                }],
                                b"race",
                                0,
                            )
                            .map(|result| matches!(result, CasResult::Applied(_)))
                    };
                    match result {
                        Ok(true) => true,
                        Err(kasu_core::Error::Invalid(message))
                            if message.contains("ref name conflicts") =>
                        {
                            false
                        }
                        other => panic!("unexpected prefix race result: {other:?}"),
                    }
                })
            })
            .collect();
        let winners = workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    });
}

#[test]
fn sqlite_ref_namespaces_cover_all_publication_paths() {
    ref_namespaces_are_transactional(&SqliteStore::memory().unwrap(), b"repo");
}

#[test]
fn sqlite_concurrent_ref_prefixes_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.db");
    SqliteStore::open(&path).unwrap();
    ref_namespace_race(|| Box::new(SqliteStore::open(&path).unwrap()), b"repo");
}

#[test]
#[cfg(feature = "postgres")]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn postgres_ref_namespaces_cover_publication_and_concurrent_prefixes() {
    let open = || {
        let mut options = PostgresOptions::new(std::env::var("KASU_TEST_POSTGRES").unwrap());
        options.allow_plaintext = true;
        PostgresStore::connect(options).unwrap()
    };
    ref_namespaces_are_transactional(&open(), kasu_core::RepoKey::generate().unwrap().as_bytes());
    ref_namespace_race(
        || Box::new(open()),
        kasu_core::RepoKey::generate().unwrap().as_bytes(),
    );
}
