use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use tower::ServiceExt;

/// Reads of the busy repository's content are slow.
struct SlowBlobs {
    inner: FsBlobStore,
    slow: HashSet<ObjectId>,
    delay: Duration,
}
impl BlobStore for SlowBlobs {
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
        if self.slow.contains(&id) {
            std::thread::sleep(self.delay);
        }
        self.inner.get_compressed(id)
    }
    fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        self.inner.put_compressed(id, bytes)
    }
}
fn publish(
    backend: &Backend,
    store: &SqliteStore,
    blobs: &dyn BlobStore,
    name: &str,
    content: &[u8],
) -> Vec<u8> {
    let key = store
        .resolve_repository_name(&RepoName::new(name).unwrap(), 0)
        .unwrap()
        .unwrap()
        .key;
    let repo = Repository { meta: store, blobs };
    let blob = repo.put_blob(content).unwrap();
    repo.associate_blob(key.as_bytes(), blob).unwrap();
    let state = backend
        .scope(Some("Bearer operator"), name.as_bytes(), Mode::Write)
        .unwrap();
    let signature = p::Signature {
        name: b"A".to_vec(),
        email: b"a@b".to_vec(),
        timestamp: 1,
        tz: b"+0000".to_vec(),
    };
    let changeset = state
        .create_changeset(p::CreateChangesetRequest {
            repo: name.as_bytes().to_vec(),
            parents: vec![],
            changes: vec![p::FileChange {
                path: b"file".to_vec(),
                entry: Some(p::Entry {
                    kind: Some(p::entry::Kind::File(p::File {
                        blob: blob.0.to_vec(),
                        executable: false,
                    })),
                }),
            }],
            metadata: Some(p::CommitMetadata {
                author: Some(signature.clone()),
                committer: Some(signature),
                message: name.as_bytes().to_vec(),
            }),
        })
        .unwrap();
    changeset.id
}
async fn read(backend: &Arc<Backend>, token: &str, name: &str, id: &[u8]) -> StatusCode {
    let body = serde_json::to_vec(&p::PathRequest {
        repo: name.as_bytes().to_vec(),
        id: id.to_vec(),
        path: b"file".to_vec(),
        ..Default::default()
    })
    .unwrap();
    let response = http_router(backend.clone())
        .oneshot(
            Request::post("/v1/read-file")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    to_bytes(response.into_body(), 1 << 20).await.unwrap();
    status
}

/// While one repository is overloaded with slow requests, a request to
/// another repository is admitted without waiting for the busy one: queued
/// busy requests do not hold server-wide slots.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn an_overloaded_repository_does_not_delay_other_repositories() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::memory().unwrap());
    let busy_content = b"busy repository content".to_vec();
    let delay = Duration::from_millis(300);
    let blobs = Arc::new(SlowBlobs {
        inner: FsBlobStore::open(temp.path()).unwrap(),
        slow: [ObjectId::hash(&busy_content)].into(),
        delay,
    });
    for name in ["team/busy", "team/quiet"] {
        let record = RepositoryRecord::new(
            RepoKey::generate().unwrap(),
            RepoName::new(name).unwrap(),
            b"sim",
            0,
        );
        store.create_repository(&record).unwrap();
    }
    let accounts = ["busy", "quiet"]
        .into_iter()
        .map(|who| RepositoryAccount {
            create_repositories: vec![],
            display_name: None,
            email: None,
            name: who.into(),
            token: format!("{who}-pat"),
            write_policy: None,
            read_directories: None,
            grants: vec![RepositoryGrant {
                admin: false,
                repositories: vec![RepoPattern::parse(&format!("team/{who}")).unwrap()],
                write_policy: None,
                read_directories: None,
            }],
        })
        .collect();
    let backend = Arc::new(
        Backend::with_registry(
            store.clone(),
            blobs.clone(),
            store.clone(),
            store.clone(),
            Some("operator".into()),
        )
        .with_repository_accounts(accounts)
        .unwrap()
        .with_limits(ServerLimits {
            active: 4,
            per_identity_active: 4,
            pending: 1024,
            queue_timeout_ms: 30_000,
            per_repository_active: Some(2),
            per_repository_pending: Some(512),
            ..Default::default()
        })
        .unwrap(),
    );
    let busy = publish(&backend, &store, blobs.as_ref(), "team/busy", &busy_content);
    let quiet = publish(&backend, &store, blobs.as_ref(), "team/quiet", b"quiet");
    // 40 slow requests take 6 s with two running at a time.
    let load: Vec<_> = (0..40)
        .map(|_| {
            let backend = backend.clone();
            let busy = busy.clone();
            tokio::spawn(async move { read(&backend, "busy-pat", "team/busy", &busy).await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut slowest = Duration::ZERO;
    for _ in 0..10 {
        let started = Instant::now();
        assert_eq!(
            read(&backend, "quiet-pat", "team/quiet", &quiet).await,
            StatusCode::OK
        );
        slowest = slowest.max(started.elapsed());
    }
    // Without per-repository admission the quiet request would queue behind
    // dozens of busy requests on the four server-wide slots.
    assert!(slowest < delay, "quiet repository waited {slowest:?}");
    let metrics = backend.metrics();
    assert!(metrics.contains("kasu_server_repository_active{repository=\"other\"}"));
    for task in load {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
}
