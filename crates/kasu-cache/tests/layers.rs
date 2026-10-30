use kasu_cache::{DiskStore, Key, Layer, Limits, MemoryStore, Service, Store};
use kasu_core::ObjectId;
use kasu_object::{Client, Kind, Layer as Role};
use kasu_proto::v1::{
    self as p,
    objects_server::{Objects, ObjectsServer},
};
use kasu_server::{Backend, ObjectService};
use kasu_store::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tonic::{Request, Response, Status};

struct Counted {
    inner: ObjectService,
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    times: Arc<Mutex<Vec<Instant>>>,
    gate: Arc<tokio::sync::Semaphore>,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
        self.times.lock().unwrap().push(Instant::now());
        let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(n, Ordering::SeqCst);
        let _guard = Active(self.active.clone());
        let _gate = self.gate.acquire().await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        if self.fail.load(Ordering::SeqCst) {
            return Err(Status::unavailable("injected upstream outage"));
        }
        self.inner.get_blobs(q).await
    }
    async fn get_repository_identity(
        &self,
        q: Request<p::RepositoryIdentityRequest>,
    ) -> Result<Response<p::RepositoryIdentityResponse>, Status> {
        self.inner.get_repository_identity(q).await
    }
}
struct Running {
    url: String,
    task: tokio::task::JoinHandle<()>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Running {
    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn start(layer: Arc<Layer>) -> Running {
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(Service::new(layer).into_server())
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = shutdown.await;
                },
            )
            .await
            .unwrap();
    });
    Running {
        url,
        task,
        stop: Some(stop),
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    blobs: Arc<FsBlobStore>,
    keys: Vec<Key>,
    authority: Running,
    calls: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    times: Arc<Mutex<Vec<Instant>>>,
    gate: Arc<tokio::sync::Semaphore>,
}
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let meta = Arc::new(SqliteStore::open(temp.path().join("meta.db")).unwrap());
        let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
        let repo = Repository {
            meta: meta.as_ref(),
            blobs: blobs.as_ref(),
        };
        let keys = (0..40)
            .map(|n| {
                let id = repo
                    .put_blob(format!("test content {n}\n").as_bytes())
                    .unwrap();
                repo.associate_blob(b"repo", id).unwrap();
                Key {
                    kind: Kind::Blob,
                    id: ObjectId(id.0),
                }
            })
            .collect();
        let state = Arc::new(
            Backend::new(
                b"repo".to_vec(),
                meta.clone(),
                blobs.clone(),
                meta,
                Some("user".into()),
            )
            .with_object_token("server-hop".into())
            .unwrap(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let times = Arc::new(Mutex::new(vec![]));
        let gate = Arc::new(tokio::sync::Semaphore::new(1024));
        let counted = Counted {
            inner: ObjectService::new(state),
            calls: calls.clone(),
            active,
            max: max.clone(),
            fail: fail.clone(),
            times: times.clone(),
            gate: gate.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ObjectsServer::new(counted))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            _temp: temp,
            blobs,
            keys,
            authority: Running {
                url,
                task,
                stop: None,
            },
            calls,
            max,
            fail,
            times,
            gate,
        }
    }
}
fn limits() -> Limits {
    Limits {
        streams: 2048,
        fills: 2,
        pending: 2048,
        qps: 10_000,
        queue_timeout: Duration::from_secs(5),
        ..Default::default()
    }
}
async fn cache(f: &Fixture, limits: Limits) -> Arc<Layer> {
    Layer::new(
        Role::Cache,
        vec![b"repo".to_vec()],
        "daemon-hop",
        "server-hop",
        Client::connect(&f.authority.url, Some("server-hop"), Some(Role::Server))
            .await
            .unwrap(),
        Arc::new(MemoryStore::new(1024 * 1024)),
        limits,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn thousand_clients_coalesce_and_cached_frames_fail_closed_without_identity_checks() {
    let f = Fixture::new().await;
    let cache = cache(&f, limits()).await;
    let regional = start(cache.clone()).await;
    let mut nodes = vec![];
    let mut layers = vec![];
    let mut stores = vec![];
    for n in 0..2 {
        let store = Arc::new(DiskStore::open(&f._temp.path().join(format!("node-{n}"))).unwrap());
        let layer = Layer::new(
            Role::Daemon,
            vec![b"repo".to_vec()],
            "user",
            "daemon-hop",
            Client::connect(&regional.url, Some("daemon-hop"), Some(Role::Cache))
                .await
                .unwrap(),
            store.clone(),
            limits(),
        )
        .unwrap();
        nodes.push(start(layer.clone()).await);
        layers.push(layer);
        stores.push(store);
    }
    let clients = [
        Client::connect(&nodes[0].url, Some("user"), Some(Role::Daemon))
            .await
            .unwrap(),
        Client::connect(&nodes[1].url, Some("user"), Some(Role::Daemon))
            .await
            .unwrap(),
    ];
    let mut joins = tokio::task::JoinSet::new();
    for n in 0..1000 {
        let client = clients[n % 2].clone();
        let key = f.keys[0];
        joins.spawn(async move { client.fetch(b"repo", key.id, key.kind).await });
    }
    let expected = f.blobs.get_compressed(f.keys[0].id).unwrap();
    while let Some(result) = joins.join_next().await {
        assert_eq!(result.unwrap().unwrap(), expected);
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(cache.stats().upstream, 1);
    assert!(cache.stats().coalesced + cache.stats().hits >= 1);
    for (store, layer) in stores.iter().zip(&layers) {
        assert_eq!(store.get(f.keys[0]).unwrap().unwrap().as_slice(), expected);
        assert_eq!(layer.stats().upstream, 1);
        assert_eq!(layer.stats().entries, 1);
    }
    let fresh = Client::connect(&regional.url, Some("daemon-hop"), Some(Role::Cache))
        .await
        .unwrap();
    assert_eq!(
        fresh
            .fetch(b"repo", f.keys[0].id, Kind::Blob)
            .await
            .unwrap(),
        expected
    );
    assert!(cache.stats().hits >= 1);
    drop(fresh);
    regional.shutdown().await;
    let before = f.calls.load(Ordering::SeqCst);
    for client in clients.iter().take(2) {
        assert_eq!(
            client
                .fetch(b"repo", f.keys[0].id, Kind::Blob)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unavailable
        );
    }
    // Neither hot nor cold frames are served without authoritative identity.
    assert!(
        clients[0]
            .fetch(b"repo", f.keys[1].id, Kind::Blob)
            .await
            .is_err()
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), before);
    assert!(cache.stats().bytes <= 1024 * 1024);
    assert!(cache.stats().max_active <= 2);
    if let Ok(path) = std::env::var("KASU_CACHE_CAPTURE") {
        let evidence = serde_json::json!({"schema":1,"clients":1000,"nodes":2,"authorityRequestsBeforeCacheStop":before,"authorityRequestsAfterCacheStop":f.calls.load(Ordering::SeqCst),"warmReadsPerNode":0,"cacheStopped":true,"cachedFramesRetainedButNotServed":true,"coldMissRejected":true,"configuredMaxFills":2,"cache":cache.stats(),"daemons":layers.iter().map(|layer|layer.stats()).collect::<Vec<_>>()});
        std::fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_misses_obey_global_concurrency_and_request_spacing() {
    let f = Fixture::new().await;
    let mut settings = limits();
    settings.qps = 25;
    let layer = cache(&f, settings).await;
    let mut joins = tokio::task::JoinSet::new();
    for key in f.keys.iter().take(12).copied() {
        let layer = layer.clone();
        joins.spawn(async move { layer.get(b"repo", key).await });
    }
    while let Some(result) = joins.join_next().await {
        result.unwrap().unwrap();
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 12);
    assert!(f.max.load(Ordering::SeqCst) <= 2);
    assert!(layer.stats().max_active <= 2);
    let times = f.times.lock().unwrap();
    assert!(times.last().unwrap().duration_since(times[0]) >= Duration::from_millis(400));
    assert!(
        layer
            .metrics()
            .contains("kasu_object_upstream_max_active{layer=\"cache\"}")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outage_opens_a_circuit_and_concurrent_requests_cannot_amplify_it() {
    let f = Fixture::new().await;
    f.fail.store(true, Ordering::SeqCst);
    let mut settings = limits();
    settings.cooldown = Duration::from_millis(250);
    settings.fills = 1;
    let layer = cache(&f, settings).await;
    for key in f.keys.iter().take(3).copied() {
        assert!(layer.get(b"repo", key).await.is_err());
    }
    let before = f.calls.load(Ordering::SeqCst);
    assert_eq!(before, 3);
    let mut tasks = tokio::task::JoinSet::new();
    for key in f.keys.iter().skip(3).take(30).copied() {
        let layer = layer.clone();
        tasks.spawn(async move { layer.get(b"repo", key).await });
    }
    while let Some(result) = tasks.join_next().await {
        assert!(result.unwrap().is_err());
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), before);
    assert!(layer.stats().circuit_rejections >= 30);
    f.fail.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(260)).await;
    assert!(layer.get(b"repo", f.keys[0]).await.is_ok());
    assert_eq!(f.calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(layer.stats().pending, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hierarchy_credentials_roles_scope_and_cancelled_leaders_are_enforced() {
    let f = Fixture::new().await;
    let key = f.keys[0];
    let user = Client::connect(&f.authority.url, Some("user"), Some(Role::Server))
        .await
        .unwrap();
    assert_eq!(
        user.fetch(b"repo", key.id, key.kind)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let wrong_role = Client::connect(&f.authority.url, Some("server-hop"), Some(Role::Cache))
        .await
        .unwrap();
    assert_eq!(
        wrong_role
            .fetch(b"repo", key.id, key.kind)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    let layer = cache(&f, limits()).await;
    let server = start(layer.clone()).await;
    let client = Client::connect(&server.url, Some("daemon-hop"), Some(Role::Cache))
        .await
        .unwrap();
    assert_eq!(
        client
            .fetch(b"other", key.id, key.kind)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    let before = f.calls.load(Ordering::SeqCst);
    let first = layer.clone();
    let leader = tokio::spawn(async move { first.get(b"repo", key).await });
    while f.calls.load(Ordering::SeqCst) == before {
        tokio::task::yield_now().await;
    }
    leader.abort();
    assert!(layer.get(b"repo", key).await.is_ok());
    assert_eq!(f.calls.load(Ordering::SeqCst), before + 1);
    // A frame cached for one repository is not a hit for another: the other
    // repository is asked upstream, which does not know the object there.
    let calls = f.calls.load(Ordering::SeqCst);
    assert_eq!(
        layer.get(b"other", key).await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), calls);
    assert!(layer.get(b"repo", key).await.is_ok());
    assert_eq!(f.calls.load(Ordering::SeqCst), calls);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_and_pending_limits_reject_excess_work_then_release_capacity() {
    let f = Fixture::new().await;
    f.gate.forget_permits(1024);
    let mut config = limits();
    config.streams = 2;
    config.pending = 2;
    let layer = cache(&f, config).await;
    let server = start(layer.clone()).await;
    let client = Client::connect(&server.url, Some("daemon-hop"), Some(Role::Cache))
        .await
        .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for key in f.keys.iter().take(2).copied() {
        let client = client.clone();
        tasks.spawn(async move { client.fetch(b"repo", key.id, key.kind).await });
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.max.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client
            .fetch(b"repo", f.keys[2].id, Kind::Blob)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::ResourceExhausted
    );
    assert_eq!(
        layer.get(b"repo", f.keys[2]).await.unwrap_err().code(),
        tonic::Code::ResourceExhausted
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    assert_eq!(layer.stats().pending, 2);
    f.gate.add_permits(2);
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    assert_eq!(f.max.load(Ordering::SeqCst), 2);
    assert_eq!(layer.stats().max_active, 2);
    assert_eq!(layer.stats().pending, 0);
    assert!(
        client
            .fetch(b"repo", f.keys[2].id, Kind::Blob)
            .await
            .is_ok()
    );
}

struct FailingWrites {
    inner: MemoryStore,
    fail: AtomicBool,
}
impl Store for FailingWrites {
    fn member(&self, repo: &[u8], key: Key) -> Result<bool, Status> {
        self.inner.member(repo, key)
    }
    fn add_member(&self, repo: &[u8], key: Key) -> Result<(), Status> {
        self.inner.add_member(repo, key)
    }
    fn member_scoped(&self, repo: &[u8], identity: &str, key: Key) -> Result<bool, Status> {
        self.inner.member_scoped(repo, identity, key)
    }
    fn add_member_scoped(&self, repo: &[u8], identity: &str, key: Key) -> Result<(), Status> {
        self.inner.add_member_scoped(repo, identity, key)
    }
    fn identify(&self, repo: &[u8], identity: &str) -> Result<(), Status> {
        self.inner.identify(repo, identity)
    }
    fn hot(&self, key: Key) -> Result<Option<kasu_cache::Frame>, Status> {
        self.inner.hot(key)
    }
    fn get(&self, key: Key) -> Result<Option<kasu_cache::Frame>, Status> {
        self.inner.get(key)
    }
    fn put(&self, key: Key, bytes: kasu_cache::Frame) -> Result<(), Status> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(Status::internal("injected cache persistence failure"));
        }
        self.inner.put(key, bytes)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_write_failures_stop_refetches_preserve_warm_hits_and_recover() {
    let f = Fixture::new().await;
    let store = Arc::new(FailingWrites {
        inner: MemoryStore::new(1024 * 1024),
        fail: AtomicBool::new(false),
    });
    let mut settings = limits();
    settings.fills = 1;
    settings.cooldown = Duration::from_secs(1);
    let layer = Layer::new(
        Role::Cache,
        vec![b"repo".to_vec()],
        "cache-hop",
        "server-hop",
        Client::connect(&f.authority.url, Some("server-hop"), Some(Role::Server))
            .await
            .unwrap(),
        store.clone(),
        settings,
    )
    .unwrap();
    let warm = layer.get(b"repo", f.keys[0]).await.unwrap();
    store.fail.store(true, Ordering::SeqCst);
    for key in f.keys.iter().skip(1).take(3).copied() {
        assert_eq!(
            layer.get(b"repo", key).await.unwrap_err().code(),
            tonic::Code::Internal
        );
    }
    let before = f.calls.load(Ordering::SeqCst);
    assert_eq!(before, 4);
    let mut requests = tokio::task::JoinSet::new();
    for key in f.keys.iter().skip(4).take(30).copied() {
        let layer = layer.clone();
        requests.spawn(async move { layer.get(b"repo", key).await });
    }
    while let Some(result) = requests.join_next().await {
        assert_eq!(
            result.unwrap().unwrap_err().code(),
            tonic::Code::Unavailable
        );
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), before);
    assert!(layer.stats().circuit_rejections >= 30);
    assert_eq!(layer.get(b"repo", f.keys[0]).await.unwrap(), warm);
    assert_eq!(f.calls.load(Ordering::SeqCst), before);
    // A successful upstream read alone must not clear the persistence fault.
    tokio::time::sleep(Duration::from_millis(1010)).await;
    assert_eq!(
        layer.get(b"repo", f.keys[1]).await.unwrap_err().code(),
        tonic::Code::Internal
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(
        layer.get(b"repo", f.keys[2]).await.unwrap_err().code(),
        tonic::Code::Unavailable
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), before + 1);
    store.fail.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1010)).await;
    let recovered = layer.get(b"repo", f.keys[1]).await.unwrap();
    assert_eq!(f.calls.load(Ordering::SeqCst), before + 2);
    assert_eq!(layer.get(b"repo", f.keys[1]).await.unwrap(), recovered);
    assert_eq!(f.calls.load(Ordering::SeqCst), before + 2);
}
