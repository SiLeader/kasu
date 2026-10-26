use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use kasu_core::*;
use kasu_derived::{Ctx, Engine};
use kasu_git::*;
use kasu_ops::Operations;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::{
    path::PathBuf,
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
struct Trace {
    paths: Mutex<Vec<String>>,
    lose: AtomicBool,
    redirect: Mutex<Option<String>>,
    aba: Mutex<Option<(Arc<SqliteStore>, ChangesetId, ChangesetId)>>,
}
async fn trace(State(state): State<Arc<Trace>>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    state.paths.lock().unwrap().push(path.clone());
    if path == "/v1/read-file"
        && let Some((meta, middle, back)) = state.aba.lock().unwrap().take()
    {
        for target in [middle, back] {
            let expected = meta.resolve(b"repo", b"refs/heads/main").unwrap();
            assert!(matches!(
                meta.compare_and_swap(
                    b"repo",
                    &[RefUpdate {
                        name: b"refs/heads/main".to_vec(),
                        expected,
                        new: Some(target)
                    }],
                    b"test-aba",
                    0
                )
                .unwrap(),
                CasResult::Applied(_)
            ));
        }
    }
    if path == "/v1/changeset"
        && let Some(target) = state.redirect.lock().unwrap().clone()
    {
        return (StatusCode::FOUND, [(axum::http::header::LOCATION, target)]).into_response();
    }
    let response = next.run(request).await;
    if path == "/v1/guarded-land" && state.lose.swap(false, Ordering::SeqCst) {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    } else {
        response
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    meta: Arc<SqliteStore>,
    blobs: Arc<FsBlobStore>,
    state: Arc<Backend>,
    url: String,
    trace: Arc<Trace>,
    server: tokio::task::JoinHandle<()>,
    root: ChangesetId,
    bad: ChangesetId,
    merged: ChangesetId,
    tip: ChangesetId,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    fn checker(&self) -> PathBuf {
        let path = self.dir.path().join("check.py");
        std::fs::write(&path,"import os,sys\nsys.path.insert(0,sys.argv[1])\nfrom kasu_bot import Client,Budget,wire,BotError\nc=Client(os.environ['KASU_METADATA_ENDPOINT'],os.environ['KASU_REPO'],os.environ['KASU_TOKEN_FILE'],Budget(5))\ntry:\n r=c.call('read-file',id=wire(os.environ['KASU_CHANGESET']),path=list(b'f'))\n failed=bytes(r['content']).startswith(b'bad')\nexcept (BotError,KeyError,TypeError,ValueError):\n sys.exit(2)\nsys.exit(int(failed))\n").unwrap();
        path
    }
    fn cs(&self, parents: &[ChangesetId], files: &[(&[u8], &[u8])], n: i64) -> ChangesetId {
        let mut engine = Engine::standard();
        register_git(&mut engine);
        let ctx = Ctx {
            repo: Repository {
                meta: self.meta.as_ref(),
                blobs: self.blobs.as_ref(),
            },
        };
        let sig = Signature {
            name: b"A".as_slice().into(),
            email: b"a@b".as_slice().into(),
            timestamp: n,
            tz: b"+0000".as_slice().into(),
        };
        let mut changes: Vec<_> = files
            .iter()
            .map(|(path, bytes)| FileChange::Set {
                path: RepoPath::new(*path).unwrap(),
                entry: Entry::File {
                    blob: ctx.repo.put_blob(bytes).unwrap(),
                    executable: false,
                },
            })
            .collect();
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        let id = Operations {
            engine: &engine,
            ctx,
        }
        .create_changeset(&Changeset {
            parents: parents.into(),
            author: sig.clone(),
            committer: sig,
            message: format!("commit {n}").into(),
            changes,
            git_provenance: None,
        })
        .unwrap();
        engine
            .derive::<GitCommit>(
                &Ctx {
                    repo: Repository {
                        meta: self.meta.as_ref(),
                        blobs: self.blobs.as_ref(),
                    },
                },
                id,
                &ProjectionId::full(b"repo".as_slice()),
            )
            .unwrap();
        id
    }
    fn publish(&self, id: ChangesetId) {
        let expected = self.meta.resolve(b"repo", b"refs/heads/main").unwrap();
        assert!(matches!(
            self.meta
                .compare_and_swap(
                    b"repo",
                    &[RefUpdate {
                        name: b"refs/heads/main".to_vec(),
                        expected,
                        new: Some(id)
                    }],
                    b"test",
                    0
                )
                .unwrap(),
            CasResult::Applied(_)
        ));
    }
    fn head(&self) -> ChangesetId {
        self.meta
            .resolve(b"repo", b"refs/heads/main")
            .unwrap()
            .target
            .unwrap()
    }
    fn read(&self, id: ChangesetId, path: &[u8]) -> Vec<u8> {
        self.state
            .read_file(p::PathRequest {
                repo: b"repo".to_vec(),
                id: id.0.to_vec(),
                path: path.to_vec(),
                ..Default::default()
            })
            .unwrap()
            .content
    }
    async fn new(lose: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta = Arc::new(SqliteStore::memory().unwrap());
        let blobs = Arc::new(FsBlobStore::open(dir.path().join("objects")).unwrap());
        let state = Arc::new(Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            blobs.clone(),
            meta.clone(),
            Some("bot-test".into()),
        ));
        let trace = Arc::new(Trace::default());
        trace.lose.store(lose, Ordering::SeqCst);
        let app: Router = http_router(state.clone())
            .layer(middleware::from_fn_with_state(trace.clone(), crate::trace));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let zero = ChangesetId([0; 32]);
        let mut f = Self {
            dir,
            meta,
            blobs,
            state,
            url,
            trace,
            server,
            root: zero,
            bad: zero,
            merged: zero,
            tip: zero,
        };
        f.root = f.cs(&[], &[(b"f", b"good\nkeep\n")], 0);
        let left = f.cs(&[f.root], &[(b"left", b"left\n")], 1);
        f.bad = f.cs(&[f.root], &[(b"f", b"bad\nkeep\n")], 2);
        f.merged = f.cs(&[left, f.bad], &[(b"f", b"bad\nkeep\n")], 3);
        f.tip = f.cs(
            &[f.merged],
            &[(b"f", b"bad\nkeep\nlater\n"), (b"other", b"preserve\n")],
            4,
        );
        f.publish(f.tip);
        std::fs::write(f.dir.path().join("token"), b"bot-test\n").unwrap();
        f
    }
    fn command(&self, journal: &str) -> Command {
        let tool =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/automation/kasu_bot.py");
        let mut cmd = Command::new("python3");
        cmd.arg(tool)
            .args(["--endpoint", &self.url, "--repo", "repo", "--token-file"])
            .arg(self.dir.path().join("token"))
            .arg("--journal")
            .arg(self.dir.path().join(journal))
            .args(["--timeout", "15"]);
        cmd
    }
    async fn run(&self, mut command: Command) -> (Output, serde_json::Value) {
        let out = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();
        let value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        (out, value)
    }
    fn revert(&self, journal: &str, bad: ChangesetId, operation: &str) -> Command {
        let mut cmd = self.command(journal);
        cmd.args([
            "revert",
            "--bad",
            &bad.to_string(),
            "--operation-id",
            operation,
        ]);
        cmd
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_only_bisect_finds_second_parent_regression_and_guarded_revert_preserves_later_edits()
 {
    let f = Fixture::new(false).await;
    let checker = f.checker();
    let mut command = f.command("bisect.json");
    command
        .args([
            "bisect",
            "--good",
            &f.root.to_string(),
            "--bad",
            &f.tip.to_string(),
            "--test-id",
            "fixture-v1",
            "--",
            "python3",
        ])
        .arg(&checker)
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/automation"));
    let (out, result) = f.run(command).await;
    assert!(out.status.success(), "{result}");
    assert_eq!(result["id"], f.bad.to_string());
    assert!(
        f.trace
            .paths
            .lock()
            .unwrap()
            .iter()
            .all(|path| matches!(path.as_str(), "/v1/changeset" | "/v1/read-file"))
    );
    let (out, result) = f.run(f.revert("revert.json", f.bad, "regression-1")).await;
    assert!(out.status.success(), "{result}");
    assert_eq!(result["status"], "landed");
    assert_eq!(f.read(f.head(), b"f"), b"good\nkeep\nlater\n");
    assert_eq!(f.read(f.head(), b"other"), b"preserve\n");
    let head = f.head();
    let count = f.meta.tail(b"repo", 0, 100).unwrap().len();
    let (out, _) = f.run(f.revert("revert.json", f.bad, "regression-1")).await;
    assert!(out.status.success());
    assert_eq!(f.head(), head);
    assert_eq!(f.meta.tail(b"repo", 0, 100).unwrap().len(), count);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_land_reply_resumes_the_same_key_even_after_branch_replacement() {
    let f = Fixture::new(true).await;
    let (out, result) = f.run(f.revert("revert.json", f.bad, "lost-reply")).await;
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(result["status"], "retry");
    assert_eq!(f.read(f.head(), b"f"), b"good\nkeep\nlater\n");
    f.publish(f.root);
    let count = f.meta.tail(b"repo", 0, 100).unwrap().len();
    let (out, result) = f.run(f.revert("revert.json", f.bad, "lost-reply")).await;
    assert!(out.status.success(), "{result}");
    assert_eq!(result["result"]["replayed"], true);
    assert_eq!(f.head(), f.root);
    assert_eq!(f.meta.tail(b"repo", 0, 100).unwrap().len(), count);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicts_mainline_and_root_reverts_are_explicit() {
    let f = Fixture::new(false).await;
    let edited = f.cs(&[f.tip], &[(b"f", b"different\nkeep\nlater\n")], 8);
    f.publish(edited);
    let (out, result) = f.run(f.revert("conflict.json", f.bad, "conflicting")).await;
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(result["status"], "conflict");
    assert_eq!(f.head(), edited);
    f.publish(f.tip);
    let (out, _) = f
        .run(f.revert("missing-mainline.json", f.merged, "merge"))
        .await;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(f.head(), f.tip);
    let mut command = f.revert("merge.json", f.merged, "merge");
    command.args(["--mainline", "1"]);
    let (out, result) = f.run(command).await;
    assert!(out.status.success(), "{result}");
    assert_eq!(f.read(f.head(), b"f"), b"good\nkeep\nlater\n");
    f.publish(f.root);
    let (out, result) = f.run(f.revert("root.json", f.root, "root")).await;
    assert!(out.status.success(), "{result}");
    assert!(
        f.state
            .list_dir(p::PathRequest {
                repo: b"repo".to_vec(),
                id: f.head().0.to_vec(),
                ..Default::default()
            })
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ancestor_guard_is_rechecked_after_cas_loss_and_bound_to_receipts() {
    let f = Fixture::new(false).await;
    let draft = f.cs(&[f.tip], &[(b"f", b"good\nkeep\nlater\n")], 20);
    let q = p::LandRequest {
        repo: b"repo".to_vec(),
        id: draft.0.to_vec(),
        name: b"refs/heads/main".to_vec(),
        idempotency_key: b"guard".to_vec(),
        required_ancestor: f.bad.0.to_vec(),
        max_attempts: 3,
        timeout_ms: 1000,
    };
    let operation = f.state.begin_land(q.clone()).unwrap();
    let LandPreparation::Ready(prepared) = f.state.prepare_land(&operation, 1).unwrap() else {
        panic!("expected prepared Land")
    };
    f.publish(f.root);
    assert!(matches!(
        f.state.commit_land(prepared, &|| false).unwrap(),
        LandAttempt::Contended(_)
    ));
    let LandPreparation::Complete(response) = f.state.prepare_land(&operation, 2).unwrap() else {
        panic!("guard must reject changed history")
    };
    assert_eq!(response.outcome, p::LandOutcome::Denied as i32);
    assert_eq!(f.head(), f.root);
    assert!(f.meta.land_record(b"repo", b"guard").unwrap().is_none());
    f.publish(f.tip);
    let response = f.state.clone().land(q.clone()).await.unwrap();
    assert_eq!(response.outcome, p::LandOutcome::Landed as i32);
    let mut changed = q.clone();
    changed.required_ancestor = f.root.0.to_vec();
    assert_eq!(
        f.state.clone().land(changed).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    f.publish(f.root);
    assert!(f.state.clone().land(q).await.unwrap().replayed);
    assert_eq!(f.head(), f.root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirects_and_unguarded_requests_fail_without_publication() {
    let f = Fixture::new(false).await;
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = hits.clone();
    let sink = Router::new().fallback(move || {
        let observed = observed.clone();
        async move {
            observed.fetch_add(1, Ordering::SeqCst);
            "unexpected redirect"
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("http://{}/v1/changeset", listener.local_addr().unwrap());
    let sink = tokio::spawn(async move {
        axum::serve(listener, sink).await.unwrap();
    });
    *f.trace.redirect.lock().unwrap() = Some(target);
    let (out, result) = f.run(f.revert("redirect.json", f.bad, "redirect")).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(result["message"].as_str().unwrap().contains("302"));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(f.head(), f.tip);
    sink.abort();
    let response = reqwest::Client::new()
        .post(format!("{}/v1/guarded-land", f.url))
        .header("Authorization", "Bearer bot-test")
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&p::LandRequest::default()).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(f.head(), f.tip);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_gates_immutable_revisions_and_detects_ref_aba_during_testing() {
    let f = Fixture::new(false).await;
    let checker =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/automation/check_file.py");
    let command = |journal: &str, id: ChangesetId, branch: bool| {
        let mut cmd = f.command(journal);
        cmd.args([
            "check",
            "--revision",
            &id.to_string(),
            "--test-id",
            "fixture-v1",
        ]);
        if branch {
            cmd.args(["--ref", "refs/heads/main"]);
        }
        cmd.args(["--", "python3"]).arg(&checker).args([
            "--path-hex",
            "66",
            "--contains-hex",
            "676f6f64",
        ]);
        cmd
    };
    let (out, result) = f.run(command("good.json", f.root, false)).await;
    assert!(out.status.success(), "{result}");
    assert_eq!(result["status"], "passed");
    let (out, result) = f.run(command("bad.json", f.tip, false)).await;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(result["status"], "failed");
    let (out, result) = f.run(command("wrong-head.json", f.root, true)).await;
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(result["status"], "stale");
    f.publish(f.root);
    let (out, result) = f.run(command("current-good.json", f.root, true)).await;
    assert!(out.status.success(), "{result}");
    f.publish(f.tip);
    let version = f.meta.resolve(b"repo", b"refs/heads/main").unwrap().version;
    *f.trace.aba.lock().unwrap() = Some((f.meta.clone(), f.root, f.tip));
    let (out, result) = f.run(command("aba.json", f.tip, true)).await;
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(result["status"], "stale");
    assert_eq!(f.head(), f.tip);
    assert_eq!(
        f.meta.resolve(b"repo", b"refs/heads/main").unwrap().version,
        version + 2
    );
    assert!(f.trace.paths.lock().unwrap().iter().all(|path| matches!(
        path.as_str(),
        "/v1/changeset" | "/v1/read-file" | "/v1/resolve-ref"
    )));
}
