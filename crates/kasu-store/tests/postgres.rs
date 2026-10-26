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
