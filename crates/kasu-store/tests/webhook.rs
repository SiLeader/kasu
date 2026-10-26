use kasu_store::*;
use std::sync::{Arc, Barrier};

#[test]
fn pending_body_and_cursor_survive_reopen_and_stale_workers_cannot_rewind() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(
        store
            .webhook_state(b"r", "hook", WebhookStream::Native)
            .unwrap(),
        WebhookState::default()
    );
    let first = store
        .prepare_webhook(b"r", "hook", WebhookStream::Native, 0, 3, b"first body")
        .unwrap();
    assert_eq!(
        store
            .prepare_webhook(
                b"r",
                "hook",
                WebhookStream::Native,
                0,
                4,
                b"concurrent body"
            )
            .unwrap(),
        first
    );
    assert!(
        store
            .finish_webhook(b"r", "hook", WebhookStream::Native, 3, Some(100))
            .unwrap()
    );
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    let state = store
        .webhook_state(b"r", "hook", WebhookStream::Native)
        .unwrap();
    assert_eq!(state.cursor, 0);
    let pending = state.pending.unwrap();
    assert_eq!(pending.body, b"first body");
    assert_eq!(pending.attempts, 1);
    assert_eq!(pending.retry_at_ms, 100);
    assert!(
        store
            .finish_webhook(b"r", "hook", WebhookStream::Native, 3, None)
            .unwrap()
    );
    let next = store
        .prepare_webhook(b"r", "hook", WebhookStream::Native, 3, 9, b"next")
        .unwrap();
    assert!(
        !store
            .finish_webhook(b"r", "hook", WebhookStream::Native, 3, Some(500))
            .unwrap()
    );
    assert!(
        !store
            .finish_webhook(b"r", "hook", WebhookStream::Native, 3, None)
            .unwrap()
    );
    assert_eq!(
        store
            .webhook_state(b"r", "hook", WebhookStream::Native)
            .unwrap(),
        next
    );
    assert_eq!(
        store
            .webhook_state(b"r", "hook", WebhookStream::Git)
            .unwrap(),
        WebhookState::default()
    );
    assert_eq!(
        store
            .webhook_state(b"other", "hook", WebhookStream::Native)
            .unwrap(),
        WebhookState::default()
    );
    assert_eq!(
        store
            .webhook_state(b"r", "other-hook", WebhookStream::Native)
            .unwrap(),
        WebhookState::default()
    );
}

#[test]
fn producers_on_independent_connections_install_one_authoritative_body() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    SqliteStore::open(&path).unwrap();
    let barrier = Arc::new(Barrier::new(8));
    let tasks: Vec<_> = (0..8)
        .map(|n| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = SqliteStore::open(path).unwrap();
                barrier.wait();
                store
                    .prepare_webhook(b"r", "hook", WebhookStream::Native, 0, 7, &[n])
                    .unwrap()
            })
        })
        .collect();
    let states: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
    assert!(states.iter().all(|s| s == &states[0]));
    let store = SqliteStore::open(path).unwrap();
    assert!(
        store
            .finish_webhook(b"r", "hook", WebhookStream::Native, 7, None)
            .unwrap()
    );
    assert_eq!(
        store
            .prepare_webhook(b"r", "hook", WebhookStream::Native, 0, 10, b"stale")
            .unwrap(),
        WebhookState {
            cursor: 7,
            pending: None
        }
    );
}
