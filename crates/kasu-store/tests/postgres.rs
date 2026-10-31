#![cfg(feature = "postgres")]
use kasu_core::*;
use kasu_store::*;
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

fn options() -> PostgresOptions {
    let mut options =
        PostgresOptions::new(std::env::var("KASU_TEST_POSTGRES").expect("KASU_TEST_POSTGRES"));
    options.allow_plaintext = true;
    options
}
fn open() -> PostgresStore {
    PostgresStore::connect(options()).unwrap()
}
fn repo(label: &str) -> Vec<u8> {
    format!(
        "pg-test/{label}/{}/{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
    .into_bytes()
}
fn native(name: &[u8], expected: RefState, n: u8) -> RefUpdate {
    RefUpdate {
        name: name.to_vec(),
        expected,
        new: Some(ChangesetId([n; 32])),
    }
}
fn git(expected: GitRefState, n: u8) -> GitRefUpdate {
    GitRefUpdate {
        name: b"refs/tags/v1".to_vec(),
        expected,
        new: Some(GitRef::Direct {
            target: GitOid([n; 20]),
            kind: GitKind::Commit,
            peeled: None,
        }),
    }
}

#[test]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn postgres_reopening_current_schema_does_not_lock_data_tables() {
    let store = open();
    let mut client = postgres::Client::connect(&options().dsn, postgres::NoTls).unwrap();
    let mut tx = client.transaction().unwrap();
    tx.batch_execute("LOCK TABLE kasu.ref_log IN ROW EXCLUSIVE MODE")
        .unwrap();
    let mut options = options();
    options.lock_timeout = std::time::Duration::from_millis(100);
    let second = PostgresStore::connect(options).unwrap();
    assert_eq!(
        second.get(b"migration-lock-test").unwrap(),
        store.get(b"migration-lock-test").unwrap()
    );
    tx.rollback().unwrap();
}

#[test]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn postgres_binary_immutable_keys_tombstones_and_reopen() {
    let store = open();
    let repo = repo("bytes");
    let mut key = repo.clone();
    key.extend(vec![0xff; 8192]);
    key.push(0);
    store.put_if_absent(&key, b"v\0\xff").unwrap();
    store.put_if_absent(&key, b"v\0\xff").unwrap();
    assert!(matches!(
        store.put_if_absent(&key, b"other"),
        Err(Error::Collision(_))
    ));
    assert_eq!(open().get(&key).unwrap(), Some(b"v\0\xff".to_vec()));
    let mut name = b"refs/heads/".to_vec();
    name.extend(vec![0xff; 8192]);
    assert!(matches!(
        store
            .compare_and_swap(
                &repo,
                &[native(&name, RefState::default(), 1)],
                b"actor\xff",
                -1
            )
            .unwrap(),
        CasResult::Applied(_)
    ));
    let first = store.resolve(&repo, &name).unwrap();
    assert_eq!(first.version, 1);
    store
        .compare_and_swap(
            &repo,
            &[RefUpdate {
                name: name.clone(),
                expected: first,
                new: None,
            }],
            b"delete",
            0,
        )
        .unwrap();
    let deleted = open().resolve(&repo, &name).unwrap();
    assert_eq!(deleted.version, 2);
    assert!(deleted.target.is_none());
    assert!(matches!(
        store
            .compare_and_swap(&repo, &[native(&name, RefState::default(), 2)], b"stale", 0)
            .unwrap(),
        CasResult::Mismatch { .. }
    ));
    assert!(store.list(&repo, b"refs/").unwrap().is_empty());
    let log = store.tail(&repo, 0, 100).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].seq, 1);
    assert_eq!(log[1].seq, 2);
    assert_eq!(log[0].actor, b"actor\xff");
    assert_eq!(store.tail(&repo, 1, 1).unwrap(), vec![log[1].clone()]);
    assert!(store.tail(b"other", 0, 100).unwrap().is_empty());
}
#[test]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn postgres_independent_connections_have_one_mixed_cas_winner() {
    let repo = repo("mixed");
    let barrier = Arc::new(Barrier::new(16));
    let workers: Vec<_> = (1..=16)
        .map(|n| {
            let repo = repo.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = open();
                barrier.wait();
                (
                    n,
                    store
                        .compare_native_and_git(
                            &repo,
                            &[native(b"refs/heads/main", RefState::default(), n)],
                            &[git(GitRefState::default(), n)],
                            &[n],
                            1,
                        )
                        .unwrap(),
                )
            })
        })
        .collect();
    let winners: Vec<_> = workers
        .into_iter()
        .filter_map(|w| {
            let (n, r) = w.join().unwrap();
            matches!(r, RefPublicationResult::Applied { .. }).then_some(n)
        })
        .collect();
    assert_eq!(winners.len(), 1);
    let n = winners[0];
    let store = open();
    assert_eq!(
        store.resolve(&repo, b"refs/heads/main").unwrap(),
        RefState {
            target: Some(ChangesetId([n; 32])),
            version: 1
        }
    );
    assert_eq!(
        store.resolve_git(&repo, b"refs/tags/v1").unwrap(),
        GitRefState {
            value: git(GitRefState::default(), n).new,
            version: 1
        }
    );
    assert_eq!(store.tail(&repo, 0, 100).unwrap().len(), 1);
    assert_eq!(store.tail_git(&repo, 0, 100).unwrap().len(), 1);
    assert_eq!(store.tail(&repo, 0, 100).unwrap()[0].actor, vec![n]);
    assert_eq!(store.tail_git(&repo, 0, 100).unwrap()[0].actor, vec![n]);
    assert!(matches!(
        store
            .compare_native_and_git(
                &repo,
                &[native(b"refs/heads/unpublished", RefState::default(), 2)],
                &[git(GitRefState::default(), 2)],
                b"stale-tag",
                2
            )
            .unwrap(),
        RefPublicationResult::GitMismatch { .. }
    ));
    assert_eq!(
        store.resolve(&repo, b"refs/heads/unpublished").unwrap(),
        RefState::default()
    );
    assert_eq!(store.tail(&repo, 0, 100).unwrap().len(), 1);
}
#[test]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn postgres_land_replay_and_cancellation_are_transactional_across_connections() {
    let repo = repo("land");
    let barrier = Arc::new(Barrier::new(8));
    let commit = LandCommit {
        name: b"main".to_vec(),
        expected: RefState::default(),
        new: Some(ChangesetId([1; 32])),
        record: LandRecord {
            fingerprint: ObjectId::hash(b"request"),
            response: b"durable-result\0\xff".to_vec(),
        },
    };
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let repo = repo.clone();
            let barrier = barrier.clone();
            let commit = commit.clone();
            std::thread::spawn(move || {
                let store = open();
                barrier.wait();
                store
                    .commit_land(&repo, b"key", &commit, b"land", 1, &|| false)
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(
                r,
                LandCommitResult::Recorded {
                    replayed: false,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(
        results
            .iter()
            .all(|r| matches!(r,LandCommitResult::Recorded {record,..} if record==&commit.record))
    );
    let store = open();
    assert_eq!(
        store.land_record(&repo, b"key").unwrap(),
        Some(commit.record.clone())
    );
    assert_eq!(store.tail(&repo, 0, 100).unwrap().len(), 1);
    let mut wrong = commit.clone();
    wrong.record.fingerprint = ObjectId::hash(b"wrong");
    assert!(
        store
            .commit_land(&repo, b"key", &wrong, b"a", 1, &|| false)
            .is_err()
    );
    let cancelled = LandCommit {
        name: b"cancelled".to_vec(),
        ..commit.clone()
    };
    let calls = AtomicUsize::new(0);
    assert_eq!(
        store
            .commit_land(&repo, b"cancelled", &cancelled, b"a", 1, &|| calls
                .fetch_add(1, Ordering::SeqCst)
                > 0)
            .unwrap(),
        LandCommitResult::Cancelled
    );
    assert_eq!(
        store.resolve(&repo, b"cancelled").unwrap(),
        RefState::default()
    );
    assert!(store.land_record(&repo, b"cancelled").unwrap().is_none());
    let terminal = LandCommit {
        name: b"main".to_vec(),
        expected: store.resolve(&repo, b"main").unwrap(),
        new: None,
        ..commit
    };
    assert!(matches!(
        store
            .commit_land(&repo, b"conflict", &terminal, b"a", 1, &|| false)
            .unwrap(),
        LandCommitResult::Recorded {
            replayed: false,
            ..
        }
    ));
    assert_eq!(store.tail(&repo, 0, 100).unwrap().len(), 1);
}
#[test]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn postgres_webhook_pending_body_survives_reopen_and_stale_workers() {
    let repo = repo("webhook");
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|n| {
            let repo = repo.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = open();
                barrier.wait();
                store
                    .prepare_webhook(&repo, "sink", WebhookStream::Native, 0, 1, &[n])
                    .unwrap()
            })
        })
        .collect();
    let states: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(states.iter().all(|s| s == &states[0]));
    let store = open();
    assert_eq!(
        store
            .webhook_state(&repo, "sink", WebhookStream::Native)
            .unwrap(),
        states[0]
    );
    assert!(
        store
            .finish_webhook(&repo, "sink", WebhookStream::Native, 1, Some(100))
            .unwrap()
    );
    assert!(
        store
            .finish_webhook(&repo, "sink", WebhookStream::Native, 1, Some(50))
            .unwrap()
    );
    let state = store
        .webhook_state(&repo, "sink", WebhookStream::Native)
        .unwrap();
    let pending = state.pending.unwrap();
    assert_eq!(pending.attempts, 2);
    assert_eq!(pending.retry_at_ms, 100);
    assert!(
        store
            .finish_webhook(&repo, "sink", WebhookStream::Native, 1, None)
            .unwrap()
    );
    let next = store
        .prepare_webhook(&repo, "sink", WebhookStream::Native, 1, 2, b"next")
        .unwrap();
    assert!(
        !store
            .finish_webhook(&repo, "sink", WebhookStream::Native, 1, None)
            .unwrap()
    );
    assert_eq!(
        open()
            .webhook_state(&repo, "sink", WebhookStream::Native)
            .unwrap(),
        next
    );
    assert_eq!(
        store
            .webhook_state(&repo, "sink", WebhookStream::Git)
            .unwrap(),
        WebhookState::default()
    );
}
#[tokio::test]
#[ignore = "requires KASU_TEST_POSTGRES"]
async fn postgres_connect_query_and_drop_from_a_current_thread_runtime() {
    let store = open();
    let key = repo("async");
    store.put_if_absent(&key, b"v").unwrap();
    assert_eq!(store.get(&key).unwrap(), Some(b"v".to_vec()));
    drop(store);
}

#[test]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn postgres_sql_failure_rolls_back_refs_logs_and_counter_then_allows_retry() {
    let store = open();
    let repo = repo("failure");
    let mut raw = postgres::Client::connect(&options().dsn, postgres::NoTls).unwrap();
    raw.batch_execute("CREATE OR REPLACE FUNCTION kasu.reject_test_log() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected late failure'; END $$; CREATE TRIGGER reject_test_log BEFORE INSERT ON kasu.ref_log FOR EACH ROW WHEN (NEW.actor=decode('70672d6661696c','hex')) EXECUTE FUNCTION kasu.reject_test_log();").unwrap();
    let native = native(b"refs/heads/main", RefState::default(), 1);
    let git = git(GitRefState::default(), 1);
    let result = store.compare_native_and_git(
        &repo,
        std::slice::from_ref(&native),
        std::slice::from_ref(&git),
        b"pg-fail",
        1,
    );
    raw.batch_execute(
        "DROP TRIGGER reject_test_log ON kasu.ref_log; DROP FUNCTION kasu.reject_test_log();",
    )
    .unwrap();
    assert!(result.is_err());
    assert_eq!(
        store.resolve(&repo, b"refs/heads/main").unwrap(),
        RefState::default()
    );
    assert_eq!(
        store.resolve_git(&repo, b"refs/tags/v1").unwrap(),
        GitRefState::default()
    );
    assert!(store.tail(&repo, 0, 100).unwrap().is_empty());
    assert!(store.tail_git(&repo, 0, 100).unwrap().is_empty());
    assert!(matches!(
        store
            .compare_native_and_git(&repo, &[native], &[git], b"retry", 2)
            .unwrap(),
        RefPublicationResult::Applied { .. }
    ));
    assert_eq!(store.tail(&repo, 0, 100).unwrap()[0].seq, 1);
    assert_eq!(store.tail_git(&repo, 0, 100).unwrap()[0].seq, 1);
}
#[test]
fn postgres_plaintext_and_invalid_configuration_fail_without_exposing_the_dsn() {
    let marker = "password-secret-marker";
    let o = PostgresOptions::new(format!("host=localhost sslmode=disable password={marker}"));
    let err = PostgresStore::connect(o)
        .err()
        .expect("plaintext must be rejected");
    assert!(!err.to_string().contains(marker));
    let o = PostgresOptions::new(format!("bad-option={marker}"));
    let err = PostgresStore::connect(o)
        .err()
        .expect("bad DSN must be rejected");
    assert!(!err.to_string().contains(marker));
}

#[test]
#[ignore = "requires KASU_TEST_POSTGRES_TLS and KASU_TEST_POSTGRES_CA"]
fn postgres_tls_verifies_the_configured_ca_and_refuses_untrusted_certificates() {
    let dsn = std::env::var("KASU_TEST_POSTGRES_TLS").expect("KASU_TEST_POSTGRES_TLS");
    assert!(PostgresStore::connect(PostgresOptions::new(dsn.clone())).is_err());
    let mut options = PostgresOptions::new(dsn);
    options.ca_file = Some(
        std::env::var_os("KASU_TEST_POSTGRES_CA")
            .expect("KASU_TEST_POSTGRES_CA")
            .into(),
    );
    let store = PostgresStore::connect(options).unwrap();
    let key = repo("tls");
    store.put_if_absent(&key, b"verified TLS").unwrap();
    assert_eq!(store.get(&key).unwrap(), Some(b"verified TLS".to_vec()));
}

fn review_write(number: u64, version: u64, head: &[u8]) -> ReviewWrite {
    ReviewWrite {
        review: ReviewRow {
            number,
            version,
            state: REVIEW_OPEN,
            target: b"refs/heads/main".to_vec(),
            head: head.to_vec(),
            author: "alice".into(),
            data: b"data".to_vec(),
        },
        expected_version: version - 1,
        revision: (number == 0).then(|| ReviewRevisionRow {
            revision: 1,
            data: b"r1".to_vec(),
        }),
        votes: vec![],
        comments: vec![],
        event: b"event".to_vec(),
        request: None,
        ref_update: None,
        time: 1,
    }
}

#[test]
#[ignore = "requires KASU_TEST_POSTGRES"]
fn reviews_share_transactions_with_refs_and_serialize_writers() {
    let store = Arc::new(open());
    let repo = repo("review");
    assert_eq!(
        store
            .write_review(&repo, &review_write(0, 1, b"a"))
            .unwrap(),
        ReviewWriteResult::Applied { number: 1 }
    );
    assert_eq!(
        store
            .write_review(&repo, &review_write(0, 1, b"a"))
            .unwrap(),
        ReviewWriteResult::Duplicate(1)
    );
    assert_eq!(
        store
            .write_review(&repo, &review_write(1, 3, b"a"))
            .unwrap(),
        ReviewWriteResult::VersionMismatch
    );
    let mut keyed = review_write(0, 1, b"b");
    keyed.request = Some(ReviewRequestRecord {
        account: "alice".into(),
        key: b"k".to_vec(),
        fingerprint: b"f".to_vec(),
        response: vec![],
    });
    assert_eq!(
        store.write_review(&repo, &keyed).unwrap(),
        ReviewWriteResult::Applied { number: 2 }
    );
    assert_eq!(
        store.write_review(&repo, &keyed).unwrap(),
        ReviewWriteResult::Replayed {
            response: 2u64.to_be_bytes().to_vec()
        }
    );
    keyed.request.as_mut().unwrap().fingerprint = b"g".to_vec();
    assert!(store.write_review(&repo, &keyed).is_err());

    // A Submit-like write: ref CAS, ref log with via, and review state at once.
    let mut submit = review_write(1, 2, b"a");
    submit.review.state = REVIEW_SUBMITTED;
    submit.ref_update = Some(ReviewRefUpdate {
        update: RefUpdate {
            name: b"refs/heads/main".to_vec(),
            expected: RefState {
                target: None,
                version: 3,
            },
            new: Some(ChangesetId([9; 32])),
        },
        actor: delegated_actor(b"alice", b"kasu-web:alice@example.com"),
    });
    assert_eq!(
        store.write_review(&repo, &submit).unwrap(),
        ReviewWriteResult::RefMismatch(RefState::default())
    );
    assert_eq!(store.review(&repo, 1).unwrap().unwrap().version, 1);
    submit.ref_update.as_mut().unwrap().update.expected = RefState::default();
    for cancel_at in [0, 1] {
        let checks = AtomicUsize::new(0);
        assert_eq!(
            store
                .write_review_cancellable(&repo, &submit, &|| {
                    checks.fetch_add(1, Ordering::SeqCst) >= cancel_at
                })
                .unwrap(),
            ReviewWriteResult::Cancelled
        );
        assert_eq!(store.review(&repo, 1).unwrap().unwrap().version, 1);
        assert_eq!(
            store.resolve(&repo, b"refs/heads/main").unwrap(),
            RefState::default()
        );
        assert!(store.tail(&repo, 0, 10).unwrap().is_empty());
        assert_eq!(store.review_log(&repo, 0, 100).unwrap().len(), 2);
    }
    assert_eq!(
        store.write_review(&repo, &submit).unwrap(),
        ReviewWriteResult::Applied { number: 1 }
    );
    assert_eq!(
        store.resolve(&repo, b"refs/heads/main").unwrap().target,
        Some(ChangesetId([9; 32]))
    );
    let log = store.tail(&repo, 0, 10).unwrap();
    assert_eq!(
        (log[0].actor.as_slice(), log[0].via.as_slice()),
        (
            b"alice".as_slice(),
            b"kasu-web:alice@example.com".as_slice()
        )
    );

    // Concurrent writers on one review: exactly one wins each version.
    let barrier = Arc::new(Barrier::new(8));
    let wins = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let (store, repo, barrier, wins) =
                (open(), repo.clone(), barrier.clone(), wins.clone());
            std::thread::spawn(move || {
                let mut write = review_write(2, 2, b"b");
                write.comments = vec![ReviewCommentRow {
                    id: i + 1,
                    author: "bob".into(),
                    draft: true,
                    data: vec![],
                }];
                barrier.wait();
                if store.write_review(&repo, &write).unwrap()
                    == (ReviewWriteResult::Applied { number: 2 })
                {
                    wins.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());
    assert_eq!(wins.load(Ordering::SeqCst), 1);
    assert_eq!(store.review_comments(&repo, 2).unwrap().len(), 1);
    let listed = store
        .list_reviews(
            &repo,
            &ReviewFilter {
                state: Some(REVIEW_OPEN),
                ..Default::default()
            },
            0,
            10,
        )
        .unwrap();
    assert_eq!(listed.iter().map(|r| r.number).collect::<Vec<_>>(), [2]);
    assert_eq!(store.review_revisions(&repo, 1).unwrap().len(), 1);
    assert!(store.review_log(&repo, 0, 100).unwrap().len() >= 4);
    // Prefix scans back short Changeset IDs.
    let key = format!("scan/{}/", String::from_utf8_lossy(&repo));
    store
        .put_if_absent(format!("{key}abc1").as_bytes(), b"")
        .unwrap();
    store
        .put_if_absent(format!("{key}abd2").as_bytes(), b"")
        .unwrap();
    assert_eq!(
        store
            .scan_prefix(format!("{key}ab").as_bytes(), 10)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .scan_prefix(format!("{key}abc").as_bytes(), 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.purge_reviews(&repo, 0).unwrap(), 0);
    assert!(store.review(&repo, 1).unwrap().is_some());
    let mut removed = 0;
    loop {
        let count = store.purge_reviews(&repo, 2).unwrap();
        assert!(count <= 2);
        removed += count;
        if count == 0 {
            break;
        }
    }
    assert!(removed > 2);
    assert!(store.review(&repo, 1).unwrap().is_none());
    assert!(store.review_revisions(&repo, 1).unwrap().is_empty());
    assert!(store.review_comments(&repo, 2).unwrap().is_empty());
    assert!(store.review_log(&repo, 0, 100).unwrap().is_empty());
    assert!(
        store
            .review_request(&repo, "alice", b"k")
            .unwrap()
            .is_none()
    );
}
