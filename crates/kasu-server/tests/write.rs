use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use kasu_core::*;
use kasu_proto::v1::{self as p, writer_client::WriterClient};
use kasu_server::*;
use kasu_store::*;
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    meta: Arc<SqliteStore>,
    blobs: Arc<FsBlobStore>,
    state: Arc<Backend>,
}
impl Fixture {
    fn new(policy: WritePolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta = Arc::new(SqliteStore::memory().unwrap());
        let blobs = Arc::new(FsBlobStore::open(dir.path()).unwrap());
        let state = Arc::new(
            Backend::new(
                b"repo".to_vec(),
                meta.clone(),
                blobs.clone(),
                meta.clone(),
                Some("token".into()),
            )
            .with_write_policy(policy),
        );
        Self {
            _dir: dir,
            meta,
            blobs,
            state,
        }
    }
    fn repo(&self) -> Repository<'_> {
        Repository {
            meta: self.meta.as_ref(),
            blobs: self.blobs.as_ref(),
        }
    }
}
fn auth<T>(value: T) -> tonic::Request<T> {
    let mut q = tonic::Request::new(value);
    q.metadata_mut()
        .insert("authorization", "Bearer token".parse().unwrap());
    q
}
fn sig() -> p::Signature {
    p::Signature {
        name: b"A\xff".to_vec(),
        email: b"a@b".to_vec(),
        timestamp: 1,
        tz: b"+0000".to_vec(),
    }
}
fn metadata() -> p::CommitMetadata {
    p::CommitMetadata {
        author: Some(sig()),
        committer: Some(sig()),
        message: b"message\xff".to_vec(),
    }
}
fn blob(bytes: &[u8]) -> Vec<u8> {
    ObjectId::hash(bytes).0.to_vec()
}
fn chunk(bytes: &[u8]) -> p::BlobUpload {
    p::BlobUpload {
        repo: b"repo".to_vec(),
        id: blob(bytes),
        offset: 0,
        data: bytes.to_vec(),
        last: true,
    }
}
fn create(parents: Vec<Vec<u8>>, bytes: &[u8]) -> p::CreateChangesetRequest {
    p::CreateChangesetRequest {
        repo: b"repo".to_vec(),
        parents,
        changes: vec![p::FileChange {
            path: b"file\xff".to_vec(),
            entry: Some(p::Entry {
                kind: Some(p::entry::Kind::File(p::File {
                    blob: blob(bytes),
                    executable: false,
                })),
            }),
        }],
        metadata: Some(metadata()),
    }
}
fn update(target: Option<Vec<u8>>, expected: p::Ref) -> p::UpdateRefRequest {
    p::UpdateRefRequest {
        repo: b"repo".to_vec(),
        name: b"refs/heads/main".to_vec(),
        target,
        expected: Some(expected),
        force: false,
    }
}
fn initial() -> p::Ref {
    p::Ref {
        name: b"refs/heads/main".to_vec(),
        target: None,
        version: 0,
    }
}

#[test]
fn write_policy_json_patterns_and_byte_path_boundaries_are_enforced() {
    let policy:WritePolicy=serde_json::from_str(r#"{"allowed_prefixes":[],"allowed_ref_patterns":["refs/heads/release/*"],"force_ref_patterns":["refs/heads/release/build-?"],"allowed_path_prefixes":["app",[100,97,116,97,47,255]]}"#).unwrap();
    assert!(policy.allows_ref(b"refs/heads/release/build-1"));
    assert!(!policy.allows_ref(b"refs/heads/main"));
    assert!(policy.allows_force(b"refs/heads/release/build-1"));
    assert!(!policy.allows_force(b"refs/heads/release/build-10"));
    assert!(policy.allows_path(b"app/config"));
    assert!(!policy.allows_path(b"application/config"));
    assert!(policy.allows_path(b"data/\xff/file"));
    assert!(!policy.allows_path(b"data/other"));
    assert!(serde_json::from_str::<WritePolicy>(r#"{"typo_allow_everything":true}"#).is_err());
    assert!(
        serde_json::from_str::<WritePolicy>("{}")
            .unwrap()
            .unrestricted_paths()
    );
    let only: WritePolicy =
        serde_json::from_str(r#"{"allowed_ref_patterns":["refs/heads/main"]}"#).unwrap();
    assert!(only.allows_ref(b"refs/heads/main"));
    assert!(!only.allows_ref(b"refs/heads/other"));
    assert!(!only.allows_ref(b"refs/tags/v1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn land_grpc_and_http_replay_the_durable_result_after_ref_moves() {
    let f = Fixture::new(WritePolicy::default());
    json(
        &f,
        "/v1/upload-blobs",
        &p::UploadBlobsHttpRequest {
            chunks: vec![chunk(b"base"), chunk(b"first"), chunk(b"later")],
        },
    )
    .await;
    let root = f.state.create_changeset(create(vec![], b"base")).unwrap();
    let first = f
        .state
        .create_changeset(create(vec![root.id.clone()], b"first"))
        .unwrap();
    let current = f
        .state
        .update_ref(update(Some(root.id), initial()))
        .unwrap()
        .actual
        .unwrap();
    let bind = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = bind.local_addr().unwrap();
    let state = f.state.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(WriteService::new(state).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(bind))
            .await
            .unwrap();
    });
    let mut client = WriterClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let q = p::LandRequest {
        required_ancestor: Vec::new(),
        repo: b"repo".to_vec(),
        id: first.id.clone(),
        name: current.name,
        idempotency_key: b"land-once".to_vec(),
        max_attempts: 3,
        timeout_ms: 1000,
        require_unborn: false,
    };
    assert_eq!(
        client.land(q.clone()).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    let landed = client.land(auth(q.clone())).await.unwrap().into_inner();
    assert_eq!(landed.outcome, p::LandOutcome::Landed as i32);
    assert_eq!(landed.id, Some(first.id.clone()));
    assert!(!landed.replayed);
    let later = f
        .state
        .create_changeset(create(vec![first.id], b"later"))
        .unwrap();
    f.state
        .update_ref(update(
            Some(later.id.clone()),
            landed.actual.clone().unwrap(),
        ))
        .unwrap();
    let (code, value) = json(&f, "/v1/land", &q).await;
    assert_eq!(code, StatusCode::OK);
    let mut replay: p::LandResponse = serde_json::from_value(value).unwrap();
    assert!(replay.replayed);
    replay.replayed = false;
    assert_eq!(replay, landed);
    assert_eq!(f.meta.tail(b"repo", 0, 100).unwrap().len(), 3);
    let mut reused = q;
    reused.id = later.id;
    assert_eq!(
        client.land(auth(reused)).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    server.abort();
}
async fn json<T: serde::Serialize>(
    f: &Fixture,
    route: &str,
    value: &T,
) -> (StatusCode, serde_json::Value) {
    let response = http_router(f.state.clone())
        .oneshot(
            Request::post(route)
                .header("authorization", "Bearer token")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grpc_upload_create_merge_rebase_and_ref_are_readable_by_git() {
    let f = Fixture::new(WritePolicy::default());
    let bind = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = bind.local_addr().unwrap();
    let state = f.state.clone();
    let grpc = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(WriteService::new(state.clone()).into_server())
            .add_service(MetadataService::new(state).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(bind))
            .await
            .unwrap();
    });
    let mut client = WriterClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    assert_eq!(
        client
            .upload_blobs(tokio_stream::iter(vec![chunk(b"unauthorized")]))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let base = b"a\nkeep\nc\n";
    let ours = b"A\nkeep\nc\n";
    let theirs = b"a\nkeep\nC\n";
    let mut first = chunk(base);
    first.data = base[..3].to_vec();
    first.last = false;
    let mut second = chunk(base);
    second.offset = 3;
    second.data = base[3..].to_vec();
    let uploaded = client
        .upload_blobs(auth(tokio_stream::iter(vec![
            first,
            second,
            chunk(ours),
            chunk(theirs),
        ])))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(uploaded.blobs.len(), 3);
    assert!(uploaded.blobs.iter().all(|r| !r.existing));
    let mut check = chunk(base);
    check.data.clear();
    assert!(
        client
            .upload_blobs(auth(tokio_stream::iter(vec![check])))
            .await
            .unwrap()
            .into_inner()
            .blobs[0]
            .existing
    );
    let root = client
        .create_changeset(auth(create(vec![], base)))
        .await
        .unwrap()
        .into_inner();
    let a = client
        .create_changeset(auth(create(vec![root.id.clone()], ours)))
        .await
        .unwrap()
        .into_inner();
    let b = client
        .create_changeset(auth(create(vec![root.id.clone()], theirs)))
        .await
        .unwrap()
        .into_inner();
    let merged = client
        .merge(auth(p::MergeRequest {
            repo: b"repo".to_vec(),
            a: a.id.clone(),
            b: b.id.clone(),
            metadata: Some(metadata()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(merged.conflicts.is_empty());
    let merged = merged.id.unwrap();
    let rebased = client
        .rebase(auth(p::RebaseRequest {
            repo: b"repo".to_vec(),
            id: a.id.clone(),
            onto: b.id.clone(),
            committer: Some(sig()),
        }))
        .await
        .unwrap()
        .into_inner()
        .id
        .unwrap();
    let merged_root = f
        .state
        .get_root_tree(p::ChangesetRequest {
            repo: b"repo".to_vec(),
            id: merged.clone(),
        })
        .unwrap();
    assert_eq!(
        merged_root,
        f.state
            .get_root_tree(p::ChangesetRequest {
                repo: b"repo".to_vec(),
                id: rebased
            })
            .unwrap()
    );
    let content = f
        .state
        .read_file(p::PathRequest {
            follow_copies: false,
            follow_moves: false,
            repo: b"repo".to_vec(),
            id: merged.clone(),
            path: b"file\xff".to_vec(),
        })
        .unwrap();
    assert_eq!(content.content, b"A\nkeep\nC\n");
    let mut forced = update(Some(root.id.clone()), initial());
    forced.force = true;
    assert_eq!(
        client.update_ref(auth(forced)).await.unwrap_err().code(),
        tonic::Code::PermissionDenied
    );
    let applied = client
        .update_ref(auth(update(Some(root.id.clone()), initial())))
        .await
        .unwrap()
        .into_inner();
    assert!(applied.applied);
    let before = applied.actual.unwrap();
    let applied = client
        .update_ref(auth(update(Some(merged.clone()), before.clone())))
        .await
        .unwrap()
        .into_inner();
    assert!(applied.applied);
    let stale = client
        .update_ref(auth(update(Some(a.id), before)))
        .await
        .unwrap()
        .into_inner();
    assert!(!stale.applied);
    assert_eq!(stale.actual, applied.actual);
    assert_eq!(
        client
            .update_ref(auth(update(Some(root.id), applied.actual.clone().unwrap())))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    assert_eq!(f.meta.tail(b"repo", 0, 100).unwrap().len(), 2);
    assert_eq!(
        client
            .update_ref(auth(update(None, applied.actual.unwrap())))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let mut engine = kasu_derived::Engine::standard();
    kasu_git::register_git(&mut engine);
    let expected_git = engine
        .derive::<kasu_git::GitCommit>(
            &kasu_derived::Ctx { repo: f.repo() },
            ChangesetId(merged.as_slice().try_into().unwrap()),
            &kasu_git::ProjectionId::full(b"repo".as_slice()),
        )
        .unwrap()
        .0;
    let bind = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = bind.local_addr().unwrap();
    let router = http_router(f.state.clone());
    let http = tokio::spawn(async move {
        axum::serve(bind, router).await.unwrap();
    });
    let dest = f._dir.path().join("clone.git");
    let output = tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "http.extraHeader=Authorization: Bearer token",
                "clone",
                "--bare",
                &format!("http://{address}/repo.git"),
            ])
            .arg(&dest)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(head.status.success());
        assert_eq!(
            head.stdout.trim_ascii(),
            expected_git.to_string().as_bytes()
        );
        std::process::Command::new("git")
            .arg("-C")
            .arg(dest)
            .args(["fsck", "--strict"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    grpc.abort();
    http.abort();
}

#[tokio::test]
async fn http_rejects_foreign_objects_malformed_uploads_and_unfinished_streams() {
    let f = Fixture::new(WritePolicy::default());
    let foreign = f.repo().put_blob(b"foreign").unwrap();
    f.repo().associate_blob(b"other", foreign).unwrap();
    let (status, _) = json(&f, "/v1/create-changeset", &create(vec![], b"foreign")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let mut check = chunk(b"foreign");
    check.data.clear();
    let (status, _) = json(
        &f,
        "/v1/upload-blobs",
        &p::UploadBlobsHttpRequest {
            chunks: vec![check],
        },
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!f.repo().has_blob_membership(b"repo", foreign).unwrap());
    let (status, _) = json(
        &f,
        "/v1/upload-blobs",
        &p::UploadBlobsHttpRequest {
            chunks: vec![chunk(b"foreign")],
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let root = f
        .state
        .create_changeset(create(vec![], b"foreign"))
        .unwrap();
    let mut cross = create(vec![root.id.clone()], b"foreign");
    cross.repo = b"other".to_vec();
    assert_eq!(
        f.state.create_changeset(cross).unwrap_err().code(),
        tonic::Code::NotFound
    );
    let foreign_cs = f
        .repo()
        .put_changeset(&Changeset {
            parents: vec![],
            author: Signature {
                name: b"A".as_slice().into(),
                email: b"a@b".as_slice().into(),
                timestamp: 0,
                tz: b"+0000".as_slice().into(),
            },
            committer: Signature {
                name: b"A".as_slice().into(),
                email: b"a@b".as_slice().into(),
                timestamp: 0,
                tz: b"+0000".as_slice().into(),
            },
            message: b"foreign".as_slice().into(),
            changes: vec![],
            git_provenance: None,
        })
        .unwrap();
    f.repo().associate_changeset(b"other", foreign_cs).unwrap();
    assert_eq!(
        f.state
            .create_changeset(create(vec![foreign_cs.0.to_vec()], b"foreign"))
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    let mut unfinished = chunk(b"unfinished");
    unfinished.last = false;
    let mut offset = chunk(b"bad offset");
    offset.offset = 1;
    let mut hash = chunk(b"hash mismatch");
    hash.id = blob(b"different");
    for q in [unfinished, offset, hash] {
        assert_eq!(
            json(
                &f,
                "/v1/upload-blobs",
                &p::UploadBlobsHttpRequest { chunks: vec![q] }
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let response = http_router(f.state.clone())
        .oneshot(
            Request::post("/v1/create-changeset")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&create(vec![], b"foreign")).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut q = update(Some(root.id), initial());
    q.name = b"refs/heads/bad..name".to_vec();
    assert_eq!(
        f.state.update_ref(q).unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
}

#[tokio::test]
async fn ref_policy_force_deletion_and_aba_are_enforced() {
    let f = Fixture::new(WritePolicy {
        force_prefixes: vec![b"refs/heads/main".to_vec()],
        allow_delete: true,
        ..Default::default()
    });
    json(
        &f,
        "/v1/upload-blobs",
        &p::UploadBlobsHttpRequest {
            chunks: vec![chunk(b"one"), chunk(b"two")],
        },
    )
    .await;
    let a = f.state.create_changeset(create(vec![], b"one")).unwrap();
    let b = f.state.create_changeset(create(vec![], b"two")).unwrap();
    let first = f
        .state
        .update_ref(update(Some(a.id.clone()), initial()))
        .unwrap()
        .actual
        .unwrap();
    assert_eq!(
        f.state
            .update_ref(update(Some(b.id.clone()), first.clone()))
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let mut forced = update(Some(b.id), first.clone());
    forced.force = true;
    let second = f.state.update_ref(forced).unwrap().actual.unwrap();
    assert_eq!(second.version, 2);
    let deleted = f
        .state
        .update_ref(update(None, second))
        .unwrap()
        .actual
        .unwrap();
    assert_eq!(deleted.version, 3);
    assert_eq!(deleted.target, None);
    let recreated = f
        .state
        .update_ref(update(Some(a.id.clone()), deleted))
        .unwrap()
        .actual
        .unwrap();
    assert_eq!(recreated.version, 4);
    let stale = f.state.update_ref(update(Some(a.id), first)).unwrap();
    assert!(!stale.applied);
    assert_eq!(stale.actual, Some(recreated));
    assert_eq!(f.meta.tail(b"repo", 0, 100).unwrap().len(), 4);
}
