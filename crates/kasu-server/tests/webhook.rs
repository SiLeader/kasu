use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::Engine as _;
use kasu_core::*;
use kasu_derived::{Ctx, Engine};
use kasu_git::*;
use kasu_ops::Operations;
use kasu_server::*;
use kasu_store::*;
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Debug, PartialEq)]
struct Delivery {
    id: String,
    signature: String,
    body: Vec<u8>,
}
struct Sink {
    deliveries: Mutex<Vec<Delivery>>,
    failures: AtomicUsize,
    delay_ms: AtomicUsize,
}
async fn sink_handler(
    State(state): State<Arc<Sink>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    assert!(matches!(
        headers["X-GitHub-Event"].to_str().unwrap(),
        "push" | "kasu_review"
    ));
    state.deliveries.lock().unwrap().push(Delivery {
        id: headers["X-GitHub-Delivery"].to_str().unwrap().into(),
        signature: headers["X-Hub-Signature-256"].to_str().unwrap().into(),
        body: body.to_vec(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(
        state.delay_ms.load(Ordering::SeqCst) as u64,
    ))
    .await;
    let mut remaining = state.failures.load(Ordering::SeqCst);
    while remaining > 0 {
        match state.failures.compare_exchange_weak(
            remaining,
            remaining - 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return StatusCode::INTERNAL_SERVER_ERROR,
            Err(actual) => remaining = actual,
        }
    }
    StatusCode::OK
}
struct Fixture {
    _temp: tempfile::TempDir,
    store: Arc<SqliteStore>,
    blobs: Arc<FsBlobStore>,
    backend: Arc<Backend>,
    root: ChangesetId,
    next: ChangesetId,
    config: WebhookConfig,
    sink: Arc<Sink>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new(failures: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteStore::open(temp.path().join("meta.db")).unwrap());
        let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
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
        let signature = Signature {
            name: b"Author\xff".as_slice().into(),
            email: b"a@b".as_slice().into(),
            timestamp: 1_700_000_000,
            tz: b"+0000".as_slice().into(),
        };
        let set = |path: &[u8], bytes: &[u8]| FileChange::Set {
            path: RepoPath::new(path).unwrap(),
            entry: Entry::File {
                blob: ops.ctx.repo.put_blob(bytes).unwrap(),
                executable: false,
            },
        };
        let create = |parents, changes, message: &[u8]| {
            let id = ops
                .create_changeset(&Changeset {
                    parents,
                    author: signature.clone(),
                    committer: signature.clone(),
                    message: message.into(),
                    changes,
                    git_provenance: None,
                })
                .unwrap();
            ops.ctx.repo.associate_changeset(b"repo", id).unwrap();
            id
        };
        let root = create(
            vec![],
            vec![
                set(b"app/a", b"one"),
                set(b"app/deleted", b"old"),
                set(b"app/\xff", b"raw"),
            ],
            b"root\xff",
        );
        let next = create(
            vec![root],
            vec![
                set(b"app/a", b"two"),
                FileChange::Delete {
                    path: RepoPath::new(b"app/deleted".as_slice()).unwrap(),
                },
                set(b"app/new", b"new"),
                set(b"app/\xff", b"raw two"),
            ],
            b"next",
        );
        store
            .compare_and_swap(
                b"repo",
                &[RefUpdate {
                    name: b"refs/heads/main".to_vec(),
                    expected: RefState::default(),
                    new: Some(root),
                }],
                b"fixture",
                10,
            )
            .unwrap();
        let backend = Arc::new(Backend::new(
            b"repo".to_vec(),
            store.clone(),
            blobs.clone(),
            store.clone(),
            None,
        ));
        let sink = Arc::new(Sink {
            deliveries: Mutex::new(vec![]),
            failures: AtomicUsize::new(failures),
            delay_ms: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/hook", post(sink_handler))
            .route(
                "/redirect",
                post(|| async { (StatusCode::FOUND, [(axum::http::header::LOCATION, "/hook")]) }),
            )
            .with_state(sink.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let secret = temp.path().join("secret");
        std::fs::write(&secret, b"webhook-test-secret").unwrap();
        let config = WebhookConfig {
            repository_url: "http://kasu.invalid/repo.git".into(),
            endpoints: vec![WebhookEndpoint {
                id: "test".into(),
                url,
                secret_file: Some(secret),
            }],
            retry_base_ms: 10,
            retry_max_ms: 100,
            ..Default::default()
        };
        Self {
            _temp: temp,
            store,
            blobs,
            backend,
            root,
            next,
            config,
            sink,
            server,
        }
    }
    fn dispatcher(&self) -> WebhookDispatcher {
        WebhookDispatcher::new(
            self.backend.clone(),
            self.store.clone(),
            self.config.clone(),
        )
        .unwrap()
    }
    fn update(&self, target: Option<ChangesetId>) {
        let expected = self.store.resolve(b"repo", b"refs/heads/main").unwrap();
        self.store
            .compare_and_swap(
                b"repo",
                &[RefUpdate {
                    name: b"refs/heads/main".to_vec(),
                    expected,
                    new: target,
                }],
                b"writer",
                20,
            )
            .unwrap();
    }
    fn payload(&self) -> Value {
        serde_json::from_slice(&self.sink.deliveries.lock().unwrap().last().unwrap().body).unwrap()
    }
}

#[test]
fn signature_matches_rfc4231_test_case_one() {
    assert_eq!(
        webhook_signature(&[0x0b; 20], b"Hi There"),
        "sha256=b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_update_and_real_git_push_feed_the_durable_outbox() {
    use kasu_proto::v1 as p;
    let f = Fixture::new(0).await;
    let dispatcher = f.dispatcher();
    dispatcher
        .dispatch_once(0, WebhookStream::Native, 0)
        .await
        .unwrap();
    let expected = f.store.resolve(b"repo", b"refs/heads/main").unwrap();
    let updated = f
        .backend
        .update_ref(p::UpdateRefRequest {
            repo: b"repo".to_vec(),
            name: b"refs/heads/main".to_vec(),
            expected: Some(p::Ref {
                name: b"refs/heads/main".to_vec(),
                target: Some(f.root.0.to_vec()),
                version: expected.version,
            }),
            target: Some(f.next.0.to_vec()),
            force: false,
        })
        .unwrap();
    assert!(updated.applied);
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 0)
            .await
            .unwrap(),
        WebhookOutcome::Acknowledged(2)
    );
    let before = f.payload()["after"].as_str().unwrap().to_owned();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/repo.git", listener.local_addr().unwrap());
    let router = http_router(f.backend.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let path = f._temp.path().join("client");
    let after = tokio::task::spawn_blocking(move || {
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_NAME", "Webhook test")
                .env("GIT_AUTHOR_EMAIL", "test@kasu.invalid")
                .env("GIT_COMMITTER_NAME", "Webhook test")
                .env("GIT_COMMITTER_EMAIL", "test@kasu.invalid")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            String::from_utf8(result.stdout).unwrap().trim().to_owned()
        };
        let path = path.to_str().unwrap();
        git(&["clone", "--no-checkout", "--branch", "main", &url, path]);
        git(&["-C", path, "read-tree", "HEAD"]);
        git(&["-C", path, "checkout-index", "--", "app/a"]);
        std::fs::write(std::path::Path::new(path).join("app/a"), b"Git push").unwrap();
        git(&["-C", path, "add", "app/a"]);
        git(&["-C", path, "commit", "-m", "Git webhook integration"]);
        git(&["-C", path, "push", "origin", "main"]);
        git(&["-C", path, "rev-parse", "HEAD"])
    })
    .await
    .unwrap();
    server.abort();
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 0)
            .await
            .unwrap(),
        WebhookOutcome::Acknowledged(3)
    );
    let payload = f.payload();
    assert_eq!(payload["before"], before);
    assert_eq!(payload["after"], after);
    assert_eq!(
        payload["commits"][0]["modified"],
        serde_json::json!(["app/a"])
    );
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 0)
            .await
            .unwrap(),
        WebhookOutcome::Idle
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_after_receipt_replays_the_body_and_redirects_are_not_followed() {
    let f = Fixture::new(0).await;
    f.sink.delay_ms.store(100, Ordering::SeqCst);
    let mut config = f.config.clone();
    config.timeout_ms = 20;
    let dispatcher = WebhookDispatcher::new(f.backend.clone(), f.store.clone(), config).unwrap();
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 0)
            .await
            .unwrap(),
        WebhookOutcome::Retrying(1)
    );
    assert_eq!(
        f.store
            .webhook_state(b"repo", "test", WebhookStream::Native)
            .unwrap()
            .cursor,
        0
    );
    f.sink.delay_ms.store(0, Ordering::SeqCst);
    assert_eq!(
        f.dispatcher()
            .dispatch_once(0, WebhookStream::Native, 1000)
            .await
            .unwrap(),
        WebhookOutcome::Acknowledged(1)
    );
    let deliveries = f.sink.deliveries.lock().unwrap().clone();
    assert_eq!(deliveries.len(), 2);
    assert_eq!(deliveries[0], deliveries[1]);
    drop(deliveries);
    let mut config = f.config.clone();
    config.endpoints[0].id = "redirect".into();
    config.endpoints[0].url = config.endpoints[0].url.replace("/hook", "/redirect");
    let dispatcher = WebhookDispatcher::new(f.backend.clone(), f.store.clone(), config).unwrap();
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 0)
            .await
            .unwrap(),
        WebhookOutcome::Retrying(1)
    );
    assert_eq!(f.sink.deliveries.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_and_restart_replay_identical_signed_payload_before_advancing() {
    let f = Fixture::new(1).await;
    let dispatcher = f.dispatcher();
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 0)
            .await
            .unwrap(),
        WebhookOutcome::Retrying(1)
    );
    let state = f
        .store
        .webhook_state(b"repo", "test", WebhookStream::Native)
        .unwrap();
    assert_eq!(state.cursor, 0);
    let pending = state.pending.unwrap();
    assert_eq!(pending.attempts, 1);
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 0)
            .await
            .unwrap(),
        WebhookOutcome::Deferred
    );
    drop(dispatcher);
    let reopened = Arc::new(SqliteStore::open(f._temp.path().join("meta.db")).unwrap());
    let backend = Arc::new(Backend::new(
        b"repo".to_vec(),
        reopened.clone(),
        f.blobs.clone(),
        reopened.clone(),
        None,
    ));
    let mut changed = f.config.clone();
    changed.repository_url = "http://changed.invalid/repo.git".into();
    let dispatcher = WebhookDispatcher::new(backend, reopened.clone(), changed).unwrap();
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, pending.retry_at_ms)
            .await
            .unwrap(),
        WebhookOutcome::Acknowledged(1)
    );
    let deliveries = f.sink.deliveries.lock().unwrap().clone();
    assert_eq!(deliveries.len(), 2);
    assert_eq!(deliveries[0], deliveries[1]);
    assert_eq!(
        deliveries[0].signature,
        webhook_signature(b"webhook-test-secret", &deliveries[0].body)
    );
    assert_eq!(deliveries[0].id.chars().nth(14), Some('8'));
    drop(deliveries);
    let payload = f.payload();
    assert_eq!(
        payload["repository"]["html_url"],
        "http://kasu.invalid/repo"
    );
    assert_eq!(payload["before"], "0".repeat(40));
    assert_eq!(payload["created"], true);
    let raw = base64::engine::general_purpose::STANDARD.encode(b"app/\xff");
    assert!(
        payload["commits"][0]["kasu"]["path_bytes"]["added"]
            .as_array()
            .unwrap()
            .contains(&Value::String(raw))
    );
    assert_eq!(
        reopened
            .webhook_state(b"repo", "test", WebhookStream::Native)
            .unwrap(),
        WebhookState {
            cursor: 1,
            pending: None
        }
    );
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 1000)
            .await
            .unwrap(),
        WebhookOutcome::Idle
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_paths_cover_forward_force_rewind_delete_and_tag_streams() {
    let f = Fixture::new(0).await;
    let dispatcher = f.dispatcher();
    dispatcher
        .dispatch_once(0, WebhookStream::Native, 0)
        .await
        .unwrap();
    f.update(Some(f.next));
    dispatcher
        .dispatch_once(0, WebhookStream::Native, 1000)
        .await
        .unwrap();
    let payload = f.payload();
    assert_eq!(payload["commits"][0]["modified"][0], "app/a");
    assert_eq!(payload["commits"][0]["added"][0], "app/new");
    assert_eq!(payload["commits"][0]["removed"][0], "app/deleted");
    assert_eq!(payload["forced"], false);
    f.update(Some(f.root));
    dispatcher
        .dispatch_once(0, WebhookStream::Native, 2000)
        .await
        .unwrap();
    let payload = f.payload();
    assert_eq!(payload["forced"], true);
    assert_eq!(payload["size"], 0);
    assert_eq!(payload["commits"][0]["distinct"], false);
    assert!(
        payload["commits"][0]["removed"]
            .as_array()
            .unwrap()
            .contains(&Value::String("app/new".into()))
    );
    f.update(None);
    dispatcher
        .dispatch_once(0, WebhookStream::Native, 3000)
        .await
        .unwrap();
    let payload = f.payload();
    assert_eq!(payload["deleted"], true);
    assert_eq!(payload["after"], "0".repeat(40));
    assert!(payload["head_commit"].is_null());
    f.store
        .compare_git(
            b"repo",
            &[GitRefUpdate {
                name: b"HEAD".to_vec(),
                expected: GitRefState::default(),
                new: Some(GitRef::Symbolic {
                    target: b"refs/heads/main".to_vec(),
                }),
            }],
            b"fixture",
            0,
        )
        .unwrap();
    let count = f.sink.deliveries.lock().unwrap().len();
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Git, 0)
            .await
            .unwrap(),
        WebhookOutcome::Acknowledged(1)
    );
    assert_eq!(f.sink.deliveries.lock().unwrap().len(), count);
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let ctx = Ctx {
        repo: Repository {
            meta: f.store.as_ref(),
            blobs: f.blobs.as_ref(),
        },
    };
    let oid = engine
        .derive::<GitCommit>(&ctx, f.root, &ProjectionId::full(b"repo".as_slice()))
        .unwrap()
        .0;
    f.store
        .compare_git(
            b"repo",
            &[GitRefUpdate {
                name: b"refs/tags/v1".to_vec(),
                expected: GitRefState::default(),
                new: Some(GitRef::Direct {
                    target: oid,
                    peeled: None,
                    kind: GitKind::Commit,
                }),
            }],
            b"writer",
            40,
        )
        .unwrap();
    dispatcher
        .dispatch_once(0, WebhookStream::Git, 4000)
        .await
        .unwrap();
    assert_eq!(f.payload()["ref"], "refs/tags/v1");
    assert_eq!(f.payload()["after"], oid.to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_body_is_not_acknowledged_and_worker_shuts_down() {
    let f = Fixture::new(0).await;
    let mut config = f.config.clone();
    config.max_payload_bytes = 1024;
    let dispatcher = WebhookDispatcher::new(f.backend.clone(), f.store.clone(), config).unwrap();
    assert_eq!(
        dispatcher
            .dispatch_once(0, WebhookStream::Native, 0)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::ResourceExhausted
    );
    assert_eq!(
        f.store
            .webhook_state(b"repo", "test", WebhookStream::Native)
            .unwrap()
            .cursor,
        0
    );
    let dispatcher = Arc::new(f.dispatcher());
    let (shutdown, stop) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(dispatcher.run(stop));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if f.store
                .webhook_state(b"repo", "test", WebhookStream::Native)
                .unwrap()
                .cursor
                == 1
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    shutdown.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn review_events_are_delivered_from_the_review_log() {
    let f = Fixture::new(0).await;
    let backend = Arc::new(
        Backend::new(
            b"repo".to_vec(),
            f.store.clone(),
            f.blobs.clone(),
            f.store.clone(),
            None,
        )
        .with_reviews(f.store.clone()),
    );
    let written = f
        .store
        .write_review(
            b"repo",
            &ReviewWrite {
                review: ReviewRow {
                    number: 0,
                    version: 1,
                    state: REVIEW_OPEN,
                    target: b"refs/heads/main".to_vec(),
                    head: f.next.0.to_vec(),
                    author: "alice".into(),
                    data: Vec::new(),
                },
                expected_version: 0,
                revision: None,
                votes: vec![],
                comments: vec![],
                event: br#"{"kind":"create","actor":"alice","detail":{}}"#.to_vec(),
                request: None,
                ref_update: None,
                time: 42,
            },
        )
        .unwrap();
    assert_eq!(written, ReviewWriteResult::Applied { number: 1 });
    let dispatcher = WebhookDispatcher::new(backend, f.store.clone(), f.config.clone()).unwrap();
    assert!(matches!(
        dispatcher
            .dispatch_once(0, WebhookStream::Review, 0)
            .await
            .unwrap(),
        WebhookOutcome::Acknowledged(_)
    ));
    let payload = f.payload();
    assert_eq!(payload["action"], "create");
    assert_eq!(payload["actor"], "alice");
    assert_eq!(payload["review"]["number"], 1);
    assert_eq!(payload["review"]["state"], "open");
    assert_eq!(payload["review"]["target"], "refs/heads/main");
    assert!(matches!(
        dispatcher
            .dispatch_once(0, WebhookStream::Review, 0)
            .await
            .unwrap(),
        WebhookOutcome::Idle
    ));
}
