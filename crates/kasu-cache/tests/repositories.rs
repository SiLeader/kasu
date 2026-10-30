use kasu_cache::{Key, Layer, Limits, MemoryStore, Store};
use kasu_core::{ObjectId, RepoKey, RepoName, RepositoryState};
use kasu_object::{Client, Kind, Layer as Role};
use kasu_proto::v1::{
    self as p,
    objects_server::{Objects, ObjectsServer},
};
use kasu_server::{Backend, ObjectService};
use kasu_store::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tonic::{Request, Response, Status};

struct Counted {
    inner: ObjectService,
    calls: Arc<AtomicUsize>,
}
#[tonic::async_trait]
impl Objects for Counted {
    type GetTreesStream = <ObjectService as Objects>::GetTreesStream;
    type GetBlobsStream = <ObjectService as Objects>::GetBlobsStream;
    async fn get_trees(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> Result<Response<Self::GetTreesStream>, Status> {
        self.inner.get_trees(q).await
    }
    async fn get_blobs(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> Result<Response<Self::GetBlobsStream>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get_blobs(q).await
    }
    async fn get_repository_identity(
        &self,
        q: Request<p::RepositoryIdentityRequest>,
    ) -> Result<Response<p::RepositoryIdentityResponse>, Status> {
        self.inner.get_repository_identity(q).await
    }
}
fn create(store: &SqliteStore, name: &str) -> RepoKey {
    let record = RepositoryRecord::new(
        RepoKey::generate().unwrap(),
        RepoName::new(name).unwrap(),
        b"test",
        1,
    );
    match store.create_repository(&record).unwrap() {
        RepositoryWrite::Applied(record) => record.key,
        other => panic!("{other:?}"),
    }
}

/// One cache shared by repositories with overlapping content: frames are
/// stored once, but each repository needs its own upstream confirmation, and
/// a name that comes to mean another repository loses its confirmations.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frames_are_shared_but_served_only_after_per_repository_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let meta = Arc::new(SqliteStore::open(temp.path().join("meta.db")).unwrap());
    let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
    let a = create(&meta, "team/a");
    let b = create(&meta, "team/b");
    let repo = Repository {
        meta: meta.as_ref(),
        blobs: blobs.as_ref(),
    };
    let blob = |content: &[u8], owners: &[&RepoKey]| {
        let id = repo.put_blob(content).unwrap();
        for owner in owners {
            repo.associate_blob(owner.as_bytes(), id).unwrap();
        }
        Key {
            kind: Kind::Blob,
            id: ObjectId(id.0),
        }
    };
    let shared = blob(b"shared", &[&a, &b]);
    let private = blob(b"only a", &[&a]);
    let backend = Backend::with_registry(
        meta.clone(),
        blobs.clone(),
        meta.clone(),
        meta.clone(),
        Some("user".into()),
    )
    .with_registry_ttl(Duration::from_millis(1))
    .with_object_token("server-hop".into())
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Counted {
        inner: ObjectService::new(Arc::new(backend)),
        calls: calls.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ObjectsServer::new(counted))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let store = Arc::new(MemoryStore::new(1024 * 1024));
    let layer = Layer::new(
        Role::Cache,
        vec![],
        "daemon-hop",
        "server-hop",
        Client::connect(&url, Some("server-hop"), Some(Role::Server))
            .await
            .unwrap(),
        store.clone(),
        Limits {
            qps: 10_000,
            ..Default::default()
        },
    )
    .unwrap();
    let upstream = || calls.load(Ordering::SeqCst);
    let frame = layer.get(b"team/a", shared).await.unwrap();
    layer.get(b"team/a", private).await.unwrap();
    assert_eq!(upstream(), 2);
    // Names are case-insensitive, so this is a warm hit for team/a.
    layer.get(b"team/a", shared).await.unwrap();
    assert_eq!(upstream(), 2);
    // The shared frame is cached, but team/b has not been confirmed for it.
    assert_eq!(layer.get(b"team/b", shared).await.unwrap(), frame);
    assert_eq!(upstream(), 3);
    layer.get(b"team/b", shared).await.unwrap();
    assert_eq!(upstream(), 3);
    // An object only team/a has is never served to team/b from the cache.
    assert_eq!(
        layer.get(b"team/b", private).await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    assert_eq!(upstream(), 4);
    let identity = store.identity(b"team/b").unwrap().unwrap();
    assert_ne!(Some(identity.clone()), store.identity(b"team/a").unwrap());

    // team/b is deleted and the name reused by a new repository that has
    // only `private`. The first upstream answer for the new identity drops
    // every confirmation of the old one.
    let mut old = meta.repository(&b).unwrap().unwrap();
    old.state = RepositoryState::Deleting;
    meta.update_repository(&old, "deleted", b"test", 2).unwrap();
    let reused = create(&meta, "team/b");
    repo.associate_blob(reused.as_bytes(), kasu_core::BlobId(private.id.0))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        layer.get(b"team/b", shared).await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    assert!(!store.member(b"team/b", shared).unwrap());
    layer.get(b"team/b", private).await.unwrap();
    assert_ne!(store.identity(b"team/b").unwrap().unwrap(), identity);
    assert!(!store.member(b"team/b", shared).unwrap());
    assert_eq!(
        layer.get(b"team/b", shared).await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    // Allowlisted layers treat other repositories as missing.
    let limited = Layer::new(
        Role::Cache,
        vec![b"Team/A".to_vec()],
        "daemon-hop",
        "server-hop",
        Client::connect(&url, Some("server-hop"), Some(Role::Server))
            .await
            .unwrap(),
        Arc::new(MemoryStore::new(1024)),
        Limits::default(),
    )
    .unwrap();
    assert!(limited.serves(b"team/a"));
    assert!(!limited.serves(b"team/b"));
}
