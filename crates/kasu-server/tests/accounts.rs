use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use kasu_core::*;
use kasu_proto::v1::{self as p, metadata_client::MetadataClient, writer_client::WriterClient};
use kasu_server::*;
use kasu_store::*;
use std::{process::Command, sync::Arc};
use tower::ServiceExt;

struct Fixture {
    dir: tempfile::TempDir,
    state: Arc<Backend>,
    meta: Arc<SqliteStore>,
    base: p::Changeset,
}
fn policy(paths: &[&[u8]], refs: &[&[u8]]) -> WritePolicy {
    WritePolicy {
        allowed_path_prefixes: paths.iter().map(|x| x.to_vec()).collect(),
        allowed_ref_patterns: refs.iter().map(|x| x.to_vec()).collect(),
        force_ref_patterns: vec![b"refs/heads/*".to_vec()],
        allow_delete: true,
        ..Default::default()
    }
}
fn metadata() -> p::CommitMetadata {
    let sig = p::Signature {
        name: b"Account fixture".to_vec(),
        email: b"account@example.invalid".to_vec(),
        timestamp: 1,
        tz: b"+0000".to_vec(),
    };
    p::CommitMetadata {
        author: Some(sig.clone()),
        committer: Some(sig),
        message: b"account write".to_vec(),
    }
}
fn create(parents: Vec<Vec<u8>>, path: &[u8], bytes: &[u8]) -> p::CreateChangesetRequest {
    p::CreateChangesetRequest {
        repo: b"repo".to_vec(),
        parents,
        changes: vec![p::FileChange {
            path: path.to_vec(),
            entry: Some(p::Entry {
                kind: Some(p::entry::Kind::File(p::File {
                    blob: ObjectId::hash(bytes).0.to_vec(),
                    executable: false,
                })),
            }),
        }],
        metadata: Some(metadata()),
    }
}
fn upload(bytes: &[u8]) -> p::UploadBlobsHttpRequest {
    p::UploadBlobsHttpRequest {
        chunks: vec![p::BlobUpload {
            repo: b"repo".to_vec(),
            id: ObjectId::hash(bytes).0.to_vec(),
            data: bytes.to_vec(),
            offset: 0,
            last: true,
        }],
    }
}
fn request<T>(token: &str, value: T) -> tonic::Request<T> {
    let mut q = tonic::Request::new(value);
    q.metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    q
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta = Arc::new(SqliteStore::memory().unwrap());
        let blobs = Arc::new(FsBlobStore::open(dir.path().join("objects")).unwrap());
        let repo = Repository {
            meta: meta.as_ref(),
            blobs: blobs.as_ref(),
        };
        let backend = Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            blobs.clone(),
            meta.clone(),
            Some("operator".into()),
        );
        for bytes in [b"base".as_slice(), b"alice", b"bob", b"outside", b"merge"] {
            let id = repo.put_blob(bytes).unwrap();
            repo.associate_blob(b"repo", id).unwrap();
        }
        let base = backend
            .create_changeset(create(vec![], b"private/config", b"base"))
            .unwrap();
        let base = backend
            .create_changeset(create(vec![base.id], b"app/config", b"base"))
            .unwrap();
        backend
            .update_ref(p::UpdateRefRequest {
                repo: b"repo".to_vec(),
                name: b"refs/heads/main".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/main".to_vec(),
                    target: None,
                    version: 0,
                }),
                target: Some(base.id.clone()),
                force: false,
            })
            .unwrap();
        let global = WritePolicy {
            allowed_path_prefixes: vec![b"app".to_vec(), b"docs".to_vec(), b"\xff".to_vec()],
            force_ref_patterns: vec![b"refs/heads/automation/*".to_vec()],
            ..Default::default()
        };
        let state = backend
            .with_write_policy(global)
            .with_repository_accounts(vec![
                RepositoryAccount {
                    read_directories: None,
                    name: "alice".into(),
                    token: "alice-pat".into(),
                    write_policy: Some(policy(
                        &[b"app", b"\xff", b"outside"],
                        &[
                            b"refs/heads/main",
                            b"refs/heads/automation/*",
                            b"refs/heads/alice",
                        ],
                    )),
                },
                RepositoryAccount {
                    read_directories: None,
                    name: "bob".into(),
                    token: "bob-pat".into(),
                    write_policy: Some(policy(
                        &[b"docs"],
                        &[
                            b"refs/heads/main",
                            b"refs/heads/automation/*",
                            b"refs/heads/bob",
                        ],
                    )),
                },
                RepositoryAccount {
                    read_directories: None,
                    name: "reader".into(),
                    token: "reader-pat".into(),
                    write_policy: None,
                },
            ])
            .unwrap();
        Self {
            dir,
            meta,
            state: Arc::new(state),
            base,
        }
    }
    async fn http<T: serde::Serialize>(
        &self,
        token: &str,
        route: &str,
        value: &T,
    ) -> (StatusCode, serde_json::Value) {
        let response = http_router(self.state.clone())
            .oneshot(
                Request::post(route)
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(value).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }
    async fn http_ok<T: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        token: &str,
        route: &str,
        value: &T,
    ) -> R {
        let (code, body) = self.http(token, route, value).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        serde_json::from_value(body).unwrap()
    }
    fn current(&self) -> p::Ref {
        let r = self.meta.resolve(b"repo", b"refs/heads/main").unwrap();
        p::Ref {
            name: b"refs/heads/main".to_vec(),
            target: r.target.map(|id| id.0.to_vec()),
            version: r.version,
        }
    }
    fn update(&self, target: Vec<u8>) -> p::UpdateRefRequest {
        p::UpdateRefRequest {
            repo: b"repo".to_vec(),
            name: b"refs/heads/main".to_vec(),
            target: Some(target),
            expected: Some(self.current()),
            force: false,
        }
    }
    fn land(&self, id: Vec<u8>) -> p::LandRequest {
        p::LandRequest {
            repo: b"repo".to_vec(),
            name: b"refs/heads/main".to_vec(),
            id,
            idempotency_key: b"shared-client-key".to_vec(),
            max_attempts: 3,
            timeout_ms: 5000,
            required_ancestor: vec![],
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_accounts_apply_intersection_final_tree_and_per_principal_land_replay() {
    let f = Fixture::new();
    let bind = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = bind.local_addr().unwrap();
    let state = f.state.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(WriteService::new(state.clone()).into_server())
            .add_service(MetadataService::new(state).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(bind))
            .await
            .unwrap();
    });
    let endpoint = format!("http://{addr}");
    let mut writer = WriterClient::connect(endpoint.clone()).await.unwrap();
    let mut reader = MetadataClient::connect(endpoint).await.unwrap();
    // Read-only credentials can read repository metadata, including paths outside write scopes.
    reader
        .read_file(request(
            "reader-pat",
            p::PathRequest {
                repo: b"repo".to_vec(),
                id: f.base.id.clone(),
                path: b"private/config".to_vec(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    for route in [
        "/v1/upload-blobs",
        "/v1/create-changeset",
        "/v1/merge",
        "/v1/rebase",
        "/v1/update-ref",
        "/v1/land",
        "/v1/guarded-land",
    ] {
        let value = match route {
            "/v1/upload-blobs" => serde_json::to_value(upload(b"alice")).unwrap(),
            "/v1/create-changeset" => {
                serde_json::to_value(create(vec![f.base.id.clone()], b"app/new", b"alice")).unwrap()
            }
            "/v1/merge" => serde_json::to_value(p::MergeRequest {
                repo: b"repo".to_vec(),
                a: f.base.id.clone(),
                b: f.base.id.clone(),
                metadata: Some(metadata()),
            })
            .unwrap(),
            "/v1/rebase" => serde_json::to_value(p::RebaseRequest {
                repo: b"repo".to_vec(),
                id: f.base.id.clone(),
                onto: f.base.id.clone(),
                committer: metadata().committer,
            })
            .unwrap(),
            "/v1/update-ref" => serde_json::to_value(f.update(f.base.id.clone())).unwrap(),
            _ => serde_json::to_value(f.land(f.base.id.clone())).unwrap(),
        };
        assert_eq!(
            f.http("reader-pat", route, &value).await.0,
            StatusCode::FORBIDDEN,
            "{route}"
        );
    }
    assert_eq!(
        writer
            .upload_blobs(request(
                "reader-pat",
                tokio_stream::iter(upload(b"alice").chunks)
            ))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    writer
        .upload_blobs(request(
            "alice-pat",
            tokio_stream::iter(upload(b"alice").chunks),
        ))
        .await
        .unwrap();
    let alice = writer
        .create_changeset(request(
            "alice-pat",
            create(vec![f.base.id.clone()], b"app/config", b"alice"),
        ))
        .await
        .unwrap()
        .into_inner();
    let before = f.current();
    assert_eq!(
        writer
            .update_ref(request("bob-pat", f.update(alice.id.clone())))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    assert_eq!(f.current(), before);
    let a = writer
        .land(request("alice-pat", f.land(alice.id.clone())))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a.outcome, p::LandOutcome::Landed as i32);
    assert!(!a.replayed);
    let mut changed = f.land(alice.id.clone());
    changed.id = f.base.id.clone();
    assert_eq!(
        writer
            .land(request("alice-pat", changed))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    let bob: p::Changeset = f
        .http_ok(
            "bob-pat",
            "/v1/create-changeset",
            &create(vec![alice.id.clone()], b"docs/readme", b"bob"),
        )
        .await;
    // The same client key denotes a separate durable operation for another account.
    let b: p::LandResponse = f
        .http_ok("bob-pat", "/v1/land", &f.land(bob.id.clone()))
        .await;
    assert_eq!(b.outcome, p::LandOutcome::Landed as i32);
    assert!(!b.replayed);
    let replay: p::LandResponse = f
        .http_ok("alice-pat", "/v1/land", &f.land(alice.id.clone()))
        .await;
    assert!(replay.replayed);
    assert_eq!(replay.id, a.id);
    assert_eq!(f.current().target, Some(bob.id.clone()));
    // Global paths remain a ceiling even if an account explicitly allows more.
    let outside = f
        .state
        .create_changeset(create(vec![bob.id.clone()], b"outside/config", b"outside"))
        .unwrap();
    assert_eq!(
        f.http("alice-pat", "/v1/update-ref", &f.update(outside.id))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let bytes: p::Changeset = f
        .http_ok(
            "alice-pat",
            "/v1/create-changeset",
            &create(vec![bob.id.clone()], b"\xff/file", b"alice"),
        )
        .await;
    let applied: p::UpdateRefResponse = f
        .http_ok("alice-pat", "/v1/update-ref", &f.update(bytes.id.clone()))
        .await;
    assert!(applied.applied);
    // A merge with an empty first-parent change list still carries a forbidden final tree.
    let bad = f
        .state
        .create_changeset(create(vec![bytes.id.clone()], b"docs/secret", b"merge"))
        .unwrap();
    let merged = f
        .state
        .create_changeset(p::CreateChangesetRequest {
            repo: b"repo".to_vec(),
            parents: vec![bad.id.clone(), bytes.id.clone()],
            changes: vec![],
            metadata: Some(metadata()),
        })
        .unwrap();
    assert_eq!(
        f.http("alice-pat", "/v1/update-ref", &f.update(merged.id.clone()))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut denied = f.land(bad.id);
    denied.idempotency_key = b"deny-final-tree".to_vec();
    let denied: p::LandResponse = f.http_ok("alice-pat", "/v1/land", &denied).await;
    assert_eq!(denied.outcome, p::LandOutcome::Denied as i32);
    assert_eq!(f.current().target, Some(bytes.id));
    // Global force/deletion ceilings also apply to an account which requests both.
    let mut force = f.update(f.base.id.clone());
    force.force = true;
    assert_eq!(
        f.http("alice-pat", "/v1/update-ref", &force).await.0,
        StatusCode::FORBIDDEN
    );
    let mut delete = f.update(f.base.id.clone());
    delete.target = None;
    assert_eq!(
        f.http("alice-pat", "/v1/update-ref", &delete).await.0,
        StatusCode::FORBIDDEN
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_http_writers_keep_independent_account_policies() {
    let f = Fixture::new();
    let a = f
        .state
        .create_changeset(create(vec![f.base.id.clone()], b"app/new", b"alice"))
        .unwrap();
    let b = f
        .state
        .create_changeset(create(vec![f.base.id.clone()], b"docs/new", b"bob"))
        .unwrap();
    for name in [b"refs/heads/alice".as_slice(), b"refs/heads/bob"] {
        f.meta
            .compare_and_swap(
                b"repo",
                &[RefUpdate {
                    name: name.to_vec(),
                    expected: RefState {
                        target: None,
                        version: 0,
                    },
                    new: Some(ChangesetId(f.base.id.as_slice().try_into().unwrap())),
                }],
                b"fixture",
                1,
            )
            .unwrap();
    }
    for _ in 0..16 {
        let mut alice = f.update(a.id.clone());
        alice.name = b"refs/heads/alice".to_vec();
        let actual = f.meta.resolve(b"repo", &alice.name).unwrap();
        alice.expected = Some(p::Ref {
            name: alice.name.clone(),
            target: actual.target.map(|id| id.0.to_vec()),
            version: actual.version,
        });
        let mut bob = f.update(b.id.clone());
        bob.name = b"refs/heads/bob".to_vec();
        let actual = f.meta.resolve(b"repo", &bob.name).unwrap();
        bob.expected = Some(p::Ref {
            name: bob.name.clone(),
            target: actual.target.map(|id| id.0.to_vec()),
            version: actual.version,
        });
        let mut wrong_alice = f.update(b.id.clone());
        let mut wrong_bob = f.update(a.id.clone());
        wrong_alice.expected = Some(f.current());
        wrong_bob.expected = Some(f.current());
        let (x, y, u, v) = tokio::join!(
            f.http("alice-pat", "/v1/update-ref", &alice),
            f.http("bob-pat", "/v1/update-ref", &bob),
            f.http("alice-pat", "/v1/update-ref", &wrong_alice),
            f.http("bob-pat", "/v1/update-ref", &wrong_bob)
        );
        assert_eq!(x.0, StatusCode::OK);
        assert_eq!(y.0, StatusCode::OK);
        assert_eq!(u.0, StatusCode::FORBIDDEN);
        assert_eq!(v.0, StatusCode::FORBIDDEN);
    }
    assert_eq!(f.current().target, Some(f.base.id.clone()));
}

fn git(path: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(path)
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
        .env("GIT_AUTHOR_NAME", "account")
        .env("GIT_AUTHOR_EMAIL", "account@example.invalid")
        .env("GIT_COMMITTER_NAME", "account")
        .env("GIT_COMMITTER_EMAIL", "account@example.invalid")
        .output()
        .unwrap()
}
fn success(o: std::process::Output) {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_http_push_uses_account_paths_and_atomic_ref_policy() {
    let f = Fixture::new();
    let bind = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = bind.local_addr().unwrap();
    let state = f.state.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            bind,
            http_router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let remote = format!("http://any:alice-pat@{addr}/repo.git");
    let target = f.dir.path().join("git");
    success(git(
        f.dir.path(),
        &[
            "clone",
            "--branch",
            "main",
            &remote,
            target.to_str().unwrap(),
        ],
    ));
    std::fs::write(target.join("app/config"), b"alice").unwrap();
    success(git(&target, &["commit", "-am", "allowed path"]));
    success(git(&target, &["push", "origin", "HEAD:main"]));
    let applied = f.current();
    success(git(&target, &["tag", "denied-tag"]));
    // The tag violates account ref policy, so atomic main+tag leaves both unchanged.
    std::fs::write(target.join("app/config"), b"next").unwrap();
    success(git(&target, &["commit", "-am", "atomic update"]));
    assert!(
        !git(
            &target,
            &[
                "push",
                "--atomic",
                "origin",
                "HEAD:main",
                "refs/tags/denied-tag"
            ]
        )
        .status
        .success()
    );
    assert_eq!(f.current(), applied);
    assert!(
        f.meta
            .resolve_git(b"repo", b"refs/tags/denied-tag")
            .unwrap()
            .value
            .is_none()
    );
    std::fs::create_dir(target.join("docs")).unwrap();
    std::fs::write(target.join("docs/forbidden"), b"denied").unwrap();
    success(git(&target, &["add", "."]));
    success(git(&target, &["commit", "-m", "forbidden path"]));
    assert!(
        !git(&target, &["push", "origin", "HEAD:main"])
            .status
            .success()
    );
    assert_eq!(f.current(), applied);
    let readonly = format!("http://any:reader-pat@{addr}/repo.git");
    assert!(
        !git(&target, &["push", &readonly, "HEAD:main"])
            .status
            .success()
    );
    success(git(&target, &["fetch", "origin"]));
    success(git(&target, &["fsck", "--strict"]));
    server.abort();
}

struct Process(std::process::Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn binary(root: &std::path::Path, config: &str) -> (Process, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let log = std::fs::File::create(root.join(format!("{config}.log"))).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_kasu-server"))
        .arg(root)
        .arg("repo")
        .arg("127.0.0.1:0")
        .arg(addr.to_string())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("KASU_TOKEN_FILE", root.join("operator.pat"))
        .env("KASU_ACCOUNTS_FILE", root.join(config))
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut child = Process(child);
    let endpoint = format!("http://{addr}");
    let client = reqwest::Client::new();
    for _ in 0..200 {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "server exited: {}",
            std::fs::read_to_string(root.join(format!("{config}.log"))).unwrap()
        );
        if client
            .get(format!("{endpoint}/metrics"))
            .bearer_auth("operator")
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return (child, endpoint);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("server readiness deadline");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_servers_preserve_account_land_receipts_across_reorder_rotation_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let meta = Arc::new(SqliteStore::open(root.join("meta.db")).unwrap());
    let blobs = Arc::new(FsBlobStore::open(root.join("objects")).unwrap());
    let backend = Backend::new(
        b"repo".to_vec(),
        meta.clone(),
        blobs.clone(),
        meta.clone(),
        Some("operator".into()),
    );
    for bytes in [b"alice".as_slice(), b"bob"] {
        let repo = Repository {
            meta: meta.as_ref(),
            blobs: blobs.as_ref(),
        };
        let id = repo.put_blob(bytes).unwrap();
        repo.associate_blob(b"repo", id).unwrap();
    }
    let base = backend
        .create_changeset(p::CreateChangesetRequest {
            repo: b"repo".to_vec(),
            parents: vec![],
            changes: create(vec![], b"private/file", b"bob").changes,
            metadata: Some(metadata()),
        })
        .unwrap();
    let first = backend
        .create_changeset(create(vec![base.id.clone()], b"app/file", b"alice"))
        .unwrap();
    let second = backend
        .create_changeset(create(vec![first.id.clone()], b"app/file", b"bob"))
        .unwrap();
    backend
        .update_ref(p::UpdateRefRequest {
            repo: b"repo".to_vec(),
            name: b"refs/heads/main".to_vec(),
            expected: Some(p::Ref {
                name: b"refs/heads/main".to_vec(),
                target: None,
                version: 0,
            }),
            target: Some(base.id),
            force: false,
        })
        .unwrap();
    for (name, token) in [
        ("operator", "operator"),
        ("alice", "alice-pat"),
        ("bob", "bob-pat"),
    ] {
        std::fs::write(root.join(format!("{name}.pat")), token).unwrap();
    }
    let alice = serde_json::json!({"name":"alice","token_file":"alice.pat","read_directories":["app"],"write_policy":{"allowed_path_prefixes":["app"]}});
    let bob = serde_json::json!({"name":"bob","token_file":"bob.pat","read_directories":["app"],"write_policy":{"allowed_path_prefixes":["app"]}});
    std::fs::write(
        root.join("one.json"),
        serde_json::json!({"accounts":[alice.clone(),bob.clone()]}).to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("two.json"),
        serde_json::json!({"accounts":[bob,alice]}).to_string(),
    )
    .unwrap();
    let (one, endpoint_one) = binary(root, "one.json").await;
    let (two, endpoint_two) = binary(root, "two.json").await;
    let client = reqwest::Client::new();
    let land = |id: Vec<u8>| p::LandRequest {
        repo: b"repo".to_vec(),
        name: b"refs/heads/main".to_vec(),
        id,
        idempotency_key: b"shared-key".to_vec(),
        max_attempts: 3,
        timeout_ms: 5000,
        required_ancestor: vec![],
    };
    async fn call(
        client: &reqwest::Client,
        endpoint: &str,
        token: &str,
        q: &p::LandRequest,
    ) -> p::LandResponse {
        let response = client
            .post(format!("{endpoint}/v1/land"))
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(q).unwrap())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.bytes().await.unwrap();
        assert!(status.is_success(), "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }
    let q = land(first.id.clone());
    let a = call(&client, &endpoint_one, "alice-pat", &q).await;
    assert_eq!(a.outcome, p::LandOutcome::Landed as i32);
    assert!(!a.replayed);
    let b = call(&client, &endpoint_two, "bob-pat", &land(second.id.clone())).await;
    assert_eq!(b.outcome, p::LandOutcome::Landed as i32);
    assert!(!b.replayed);
    let root_request = p::ChangesetRequest {
        repo: b"repo".to_vec(),
        id: first.id.clone(),
    };
    let view: p::RootTreeResponse = serde_json::from_slice(
        &client
            .post(format!("{endpoint_one}/v1/root-tree"))
            .bearer_auth("alice-pat")
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&root_request).unwrap())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(view.read_filtered);
    assert_ne!(
        view.id,
        backend.get_root_tree(root_request.clone()).unwrap().id
    );
    drop(one);
    std::fs::write(root.join("alice.pat"), "rotated-alice-pat").unwrap();
    let (restarted, endpoint) = binary(root, "two.json").await;
    let persisted = client
        .post(format!("{endpoint}/v1/blob-metadata"))
        .bearer_auth("rotated-alice-pat")
        .header("content-type", "application/json")
        .body(
            serde_json::to_vec(&p::ObjectRequest {
                repo: b"repo".to_vec(),
                ids: vec![ObjectId::hash(b"alice").0.to_vec()],
            })
            .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert!(persisted.status().is_success());
    let reopened: p::RootTreeResponse = serde_json::from_slice(
        &client
            .post(format!("{endpoint}/v1/root-tree"))
            .bearer_auth("rotated-alice-pat")
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&root_request).unwrap())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reopened, view);
    let replay = call(&client, &endpoint, "rotated-alice-pat", &q).await;
    assert!(replay.replayed);
    assert_eq!(replay.id, a.id);
    assert_eq!(
        meta.resolve(b"repo", b"refs/heads/main")
            .unwrap()
            .target
            .unwrap()
            .0
            .to_vec(),
        second.id
    );
    let logs = meta.tail(b"repo", 0, 100).unwrap();
    assert_eq!(logs.iter().filter(|l| l.actor == b"alice").count(), 1);
    assert_eq!(logs.iter().filter(|l| l.actor == b"bob").count(), 1);
    let stale = client
        .post(format!("{endpoint}/v1/land"))
        .bearer_auth("alice-pat")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&q).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), reqwest::StatusCode::UNAUTHORIZED);
    drop(restarted);
    drop(two);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directory_read_views_filter_metadata_objects_and_writer_blob_references() {
    use p::objects_client::ObjectsClient;
    let mut f = Fixture::new();
    let mut head = f.base.clone();
    for (path, bytes) in [
        (b"app/public/file".as_slice(), b"alice".as_slice()),
        (b"app/hidden/file", b"outside"),
        (b"docs/private", b"bob"),
        (b"\xff/file", b"alice"),
    ] {
        head = f
            .state
            .create_changeset(create(vec![head.id], path, bytes))
            .unwrap();
    }
    let full_root = f
        .state
        .get_root_tree(p::ChangesetRequest {
            repo: b"repo".to_vec(),
            id: head.id.clone(),
        })
        .unwrap()
        .id;
    let root_dir = f.dir.path().join("objects");
    let blobs = FsBlobStore::open(root_dir).unwrap();
    let repo = Repository {
        meta: f.meta.as_ref(),
        blobs: &blobs,
    };
    let full_tree = repo
        .tree(TreeId(full_root.as_slice().try_into().unwrap()))
        .unwrap();
    let app = match full_tree.entries[b"app".as_slice()] {
        TreeEntry::SubTree(id) => id,
        _ => panic!(),
    };
    let limited = || RepositoryAccount {
        name: "limited".into(),
        token: "limited-pat".into(),
        write_policy: Some(WritePolicy::default()),
        read_directories: Some(vec![
            RepoPath::new(b"app/public".as_slice()).unwrap(),
            RepoPath::new(b"\xff".as_slice()).unwrap(),
        ]),
    };
    f.state = Arc::new(
        (*f.state)
            .clone()
            .with_repository_accounts(vec![limited()])
            .unwrap()
            .with_object_token("hop".into())
            .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let state = f.state.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(MetadataService::new(state.clone()).into_server())
            .add_service(WriteService::new(state.clone()).into_server())
            .add_service(ObjectService::new(state).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let mut metadata = MetadataClient::connect(endpoint.clone()).await.unwrap();
    let mut objects = ObjectsClient::connect(endpoint.clone()).await.unwrap();
    let mut writer = WriterClient::connect(endpoint).await.unwrap();
    let q = p::ChangesetRequest {
        repo: b"repo".to_vec(),
        id: head.id.clone(),
    };
    let filtered = metadata
        .get_root_tree(request("limited-pat", q.clone()))
        .await
        .unwrap()
        .into_inner();
    assert!(filtered.read_filtered);
    assert_ne!(filtered.id, full_root);
    let outside_only = f
        .state
        .create_changeset(create(vec![head.id.clone()], b"app/hidden/file", b"bob"))
        .unwrap();
    let unchanged = metadata
        .get_root_tree(request(
            "limited-pat",
            p::ChangesetRequest {
                repo: b"repo".to_vec(),
                id: outside_only.id,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(unchanged.id, filtered.id);
    let object = |ids: Vec<Vec<u8>>| p::ObjectRequest {
        repo: b"repo".to_vec(),
        ids,
    };
    for id in [full_root.clone(), app.0.to_vec()] {
        assert_eq!(
            objects
                .get_trees(request("limited-pat", object(vec![id])))
                .await
                .err()
                .unwrap()
                .code(),
            tonic::Code::NotFound
        );
    }
    let mut stream = objects
        .get_trees(request("limited-pat", object(vec![filtered.id.clone()])))
        .await
        .unwrap()
        .into_inner();
    let mut compressed = vec![];
    while let Some(chunk) = stream.message().await.unwrap() {
        compressed.extend(chunk.data);
    }
    let bytes = decompress(&compressed).unwrap();
    assert_eq!(ObjectId::hash(&bytes).0.to_vec(), filtered.id);
    let tree = Tree::decode(&bytes).unwrap();
    assert_eq!(tree.entries.len(), 2);
    assert!(tree.entries.contains_key(b"app".as_slice()));
    assert!(tree.entries.contains_key(b"\xff".as_slice()));
    let path = |path: &[u8]| p::PathRequest {
        repo: b"repo".to_vec(),
        id: head.id.clone(),
        path: path.to_vec(),
        ..Default::default()
    };
    let listed = metadata
        .list_dir(request("limited-pat", path(b"app")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.entries.len(), 1);
    assert_eq!(listed.entries[0].name, b"public");
    let cs = metadata
        .get_changeset(request("limited-pat", q.clone()))
        .await
        .unwrap()
        .into_inner();
    assert!(cs.read_filtered);
    assert_eq!(cs.id, head.id);
    assert!(cs.raw_git_commit.is_none());
    assert!(cs.changes.iter().all(|c| c.path.starts_with(b"\xff/")));
    for route in ["/v1/read-file", "/v1/blame", "/v1/list-dir"] {
        assert_eq!(
            f.http("limited-pat", route, &path(b"app/hidden/file"))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let (code, body) = f
        .http("limited-pat", "/v1/read-file", &path(b"app/public/file"))
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(bytes_from_json(&body["content"]), b"alice");
    let public = ObjectId::hash(b"alice").0.to_vec();
    let private = ObjectId::hash(b"outside").0.to_vec();
    metadata
        .get_blob_metadata(request("limited-pat", object(vec![public.clone()])))
        .await
        .unwrap();
    assert_eq!(
        metadata
            .get_blob_metadata(request("limited-pat", object(vec![private.clone()])))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    assert_eq!(
        objects
            .get_blobs(request(
                "limited-pat",
                object(vec![public.clone(), private.clone()])
            ))
            .await
            .err()
            .unwrap()
            .code(),
        tonic::Code::NotFound
    );
    // A hop still reads the repository; full-reader PATs still cannot replace the hop credential.
    objects
        .get_blobs(request("hop", object(vec![private.clone()])))
        .await
        .unwrap();
    assert_eq!(
        objects
            .get_blobs(request("operator", object(vec![private.clone()])))
            .await
            .err()
            .unwrap()
            .code(),
        tonic::Code::Unauthenticated
    );
    let diff = metadata
        .diff(request(
            "limited-pat",
            p::DiffRequest {
                repo: b"repo".to_vec(),
                base: f.base.id.clone(),
                head: head.id.clone(),
                paths: vec![],
                content: true,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(diff.files.len(), 2);
    assert!(
        diff.files
            .iter()
            .all(|f| f.path.starts_with(b"app/public/") || f.path.starts_with(b"\xff/"))
    );
    let log = metadata
        .log(request(
            "limited-pat",
            p::LogRequest {
                repo: b"repo".to_vec(),
                start: head.id.clone(),
                path: None,
                limit: 100,
                cursor: vec![],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(log.changesets.iter().all(|c| {
        c.read_filtered
            && c.changes
                .iter()
                .all(|f| f.path.starts_with(b"app/public/") || f.path.starts_with(b"\xff/"))
    }));
    // Knowing a private ID must not allow copying its bytes into a readable path.
    assert_eq!(
        writer
            .create_changeset(request(
                "limited-pat",
                create(vec![head.id.clone()], b"app/public/copied", b"outside")
            ))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    let probe = p::BlobUpload {
        repo: b"repo".to_vec(),
        id: private,
        offset: 0,
        data: vec![],
        last: true,
    };
    assert_eq!(
        writer
            .upload_blobs(request("limited-pat", tokio_stream::iter(vec![probe])))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    writer
        .upload_blobs(request(
            "limited-pat",
            tokio_stream::iter(upload(b"uploaded").chunks),
        ))
        .await
        .unwrap();
    let draft = writer
        .create_changeset(request(
            "limited-pat",
            create(vec![head.id.clone()], b"app/public/new", b"uploaded"),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(draft.read_filtered);
    assert_eq!(
        writer
            .create_changeset(request(
                "limited-pat",
                create(vec![head.id.clone()], b"app/hidden/new", b"alice")
            ))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let left = f
        .state
        .create_changeset(create(vec![head.id.clone()], b"app/hidden/file", b"alice"))
        .unwrap();
    let right = f
        .state
        .create_changeset(create(vec![head.id.clone()], b"app/hidden/file", b"bob"))
        .unwrap();
    let conflict = writer
        .merge(request(
            "limited-pat",
            p::MergeRequest {
                repo: b"repo".to_vec(),
                a: left.id,
                b: right.id,
                metadata: Some(crate::metadata()),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(conflict.code(), tonic::Code::PermissionDenied);
    assert!(!conflict.message().contains("app/hidden"));
    // Git full-repo read is not available; a permitted projection is.
    let advert = |route: &str| {
        Request::get(route)
            .header("authorization", "Bearer limited-pat")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        http_router(f.state.clone())
            .oneshot(advert("/repo.git/info/refs?service=git-upload-pack"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http_router(f.state.clone())
            .oneshot(advert(
                "/repo/-/path/app/public.git/info/refs?service=git-upload-pack"
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    server.abort();
    // Scope changes select a new persistent grant namespace, even for the same account name/PAT.
    let mut narrowed = limited();
    narrowed.read_directories = Some(vec![RepoPath::new(b"docs".as_slice()).unwrap()]);
    let state = Arc::new(
        (*f.state)
            .clone()
            .with_repository_accounts(vec![narrowed])
            .unwrap(),
    );
    let service = MetadataService::new(state);
    use p::metadata_server::Metadata;
    assert_eq!(
        service
            .get_blob_metadata(request("limited-pat", object(vec![public])))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
}
fn bytes_from_json(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect()
}
