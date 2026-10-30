//! Seeded cache faults over production Layers and Objects handlers on in-memory HTTP/2.
use kasu_cache::{DiskStore, Frame, Key, Layer, Limits, MemoryStore, Service, Store};
use kasu_core::ObjectId;
use kasu_object::{Client, Kind, Layer as Role};
use kasu_proto::v1::{
    self as p,
    objects_server::{Objects, ObjectsServer},
};
use kasu_server::{Backend, ObjectService};
use kasu_store::{BlobStore, FsBlobStore, Repository, SqliteStore};
use serde::Serialize;
use std::{
    future::{Future, poll_fn},
    io,
    pin::{Pin, pin},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::{Semaphore, mpsc},
    time::Instant,
};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tonic::{
    Request, Response, Status,
    transport::{Channel, Endpoint, Server, server::Connected},
};

const REPO: &[u8] = b"cache-sim";

// Keep the executor runnable while real blocking store operations finish. Tokio
// must never auto-advance virtual time because an OS worker has not run yet.
async fn spin<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut polls = 0_u64;
    poll_fn(|cx| {
        if let Poll::Ready(value) = future.as_mut().poll(cx) {
            return Poll::Ready(value);
        }
        polls += 1;
        assert!(
            polls < 20_000_000,
            "simulation made no progress without a clock event"
        );
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}
async fn until(mut condition: impl FnMut() -> bool) {
    spin(poll_fn(|_| {
        if condition() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }))
    .await;
}
async fn tick(ms: u64) {
    tokio::time::advance(Duration::from_millis(ms)).await;
}

struct Wire(DuplexStream);
impl Connected for Wire {
    type ConnectInfo = ();
    fn connect_info(&self) {}
}
impl AsyncRead for Wire {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}
impl AsyncWrite for Wire {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}
struct Peer {
    channel: Channel,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn peer<T: Objects>(service: T) -> Peer {
    let (tx, rx) = mpsc::channel::<Wire>(4);
    let task = tokio::spawn(async move {
        Server::builder()
            .add_service(ObjectsServer::new(service))
            .serve_with_incoming(ReceiverStream::new(rx).map(Ok::<_, io::Error>))
            .await
            .unwrap();
    });
    let connector = tower::service_fn(move |_| {
        let tx = tx.clone();
        async move {
            let (client, server) = tokio::io::duplex(1024 * 1024);
            tx.send(Wire(server))
                .await
                .map_err(|_| io::Error::other("simulated endpoint stopped"))?;
            Ok::<_, io::Error>(hyper_util::rt::TokioIo::new(client))
        }
    });
    let channel = spin(
        Endpoint::from_static("http://cache-simulation.invalid").connect_with_connector(connector),
    )
    .await
    .unwrap();
    Peer { channel, task }
}

struct Gate {
    hold: AtomicBool,
    permits: Semaphore,
}
impl Default for Gate {
    fn default() -> Self {
        Self {
            hold: AtomicBool::new(false),
            permits: Semaphore::new(0),
        }
    }
}
impl Gate {
    fn held() -> Self {
        Self {
            hold: AtomicBool::new(true),
            permits: Semaphore::new(0),
        }
    }
    async fn wait(&self) {
        if self.hold.load(Ordering::SeqCst) {
            self.permits.acquire().await.unwrap().forget();
        }
    }
    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        self.permits.add_permits(1);
    }
}
struct Authority {
    inner: ObjectService,
    gate: Gate,
    mode: AtomicU8, // 0 healthy, 1 truncated stream, 2 stalled RPC
    calls: AtomicU64,
    sleeping: AtomicU64,
    truncated: AtomicU64,
    stalled: AtomicU64,
}
#[derive(Clone)]
struct AuthorityRpc(Arc<Authority>);
#[tonic::async_trait]
impl Objects for AuthorityRpc {
    type GetTreesStream = ReceiverStream<std::result::Result<p::TreeObject, Status>>;
    type GetBlobsStream = ReceiverStream<std::result::Result<p::BlobChunk, Status>>;
    async fn get_trees(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> std::result::Result<Response<Self::GetTreesStream>, Status> {
        self.0.inner.get_trees(q).await
    }
    async fn get_blobs(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> std::result::Result<Response<Self::GetBlobsStream>, Status> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0.gate.wait().await;
        let mode = self.0.mode.load(Ordering::SeqCst);
        if mode == 2 {
            self.0.stalled.fetch_add(1, Ordering::SeqCst);
            return std::future::pending().await;
        }
        if mode == 0 {
            self.0.sleeping.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let response = self.0.inner.get_blobs(q).await?;
        if mode != 1 {
            return Ok(response);
        }
        self.0.truncated.fetch_add(1, Ordering::SeqCst);
        let (metadata, mut stream, extensions) = response.into_parts();
        let (tx, rx) = mpsc::channel(2);
        tokio::spawn(async move {
            let first = stream.next().await.unwrap().unwrap();
            assert!(
                !first.last,
                "fault must interrupt a genuinely incomplete frame"
            );
            let _ = tx.send(Ok(first)).await;
            let _ = tx
                .send(Err(Status::unavailable("simulated lost stream reply")))
                .await;
        });
        Ok(Response::from_parts(
            metadata,
            ReceiverStream::new(rx),
            extensions,
        ))
    }
    async fn get_repository_identity(
        &self,
        q: Request<p::RepositoryIdentityRequest>,
    ) -> std::result::Result<Response<p::RepositoryIdentityResponse>, Status> {
        self.0.inner.get_repository_identity(q).await
    }
}
struct CacheRpcState {
    current: Mutex<Option<Arc<Layer>>>,
    gate: Gate,
    calls: AtomicU64,
}
#[derive(Clone)]
struct CacheRpc(Arc<CacheRpcState>);
#[tonic::async_trait]
impl Objects for CacheRpc {
    type GetTreesStream = ReceiverStream<std::result::Result<p::TreeObject, Status>>;
    type GetBlobsStream = ReceiverStream<std::result::Result<p::BlobChunk, Status>>;
    async fn get_trees(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> std::result::Result<Response<Self::GetTreesStream>, Status> {
        let layer = self
            .0
            .current
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Status::unavailable("simulated cache stopped"))?;
        Service::new(layer).get_trees(q).await
    }
    async fn get_blobs(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> std::result::Result<Response<Self::GetBlobsStream>, Status> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0.gate.wait().await;
        let layer = self
            .0
            .current
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Status::unavailable("simulated cache stopped"))?;
        Service::new(layer).get_blobs(q).await
    }
    async fn get_repository_identity(
        &self,
        q: Request<p::RepositoryIdentityRequest>,
    ) -> std::result::Result<Response<p::RepositoryIdentityResponse>, Status> {
        let layer = self
            .0
            .current
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Status::unavailable("simulated cache stopped"))?;
        Service::new(layer).get_repository_identity(q).await
    }
}
struct WriteFault {
    inner: MemoryStore,
    fail: AtomicBool,
    failures: AtomicU64,
}
impl WriteFault {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(8 * 1024 * 1024),
            fail: AtomicBool::new(false),
            failures: AtomicU64::new(0),
        }
    }
}
impl Store for WriteFault {
    fn member(&self, repo: &[u8], key: Key) -> std::result::Result<bool, Status> {
        self.inner.member(repo, key)
    }
    fn add_member(&self, repo: &[u8], key: Key) -> std::result::Result<(), Status> {
        self.inner.add_member(repo, key)
    }
    fn identify(&self, repo: &[u8], identity: &str) -> std::result::Result<(), Status> {
        self.inner.identify(repo, identity)
    }
    fn hot(&self, key: Key) -> std::result::Result<Option<Frame>, Status> {
        self.inner.hot(key)
    }
    fn get(&self, key: Key) -> std::result::Result<Option<Frame>, Status> {
        self.inner.get(key)
    }
    fn put(&self, key: Key, bytes: Frame) -> std::result::Result<(), Status> {
        if self.fail.load(Ordering::SeqCst) {
            self.failures.fetch_add(1, Ordering::SeqCst);
            return Err(Status::internal("simulated cache persistence failure"));
        }
        self.inner.put(key, bytes)
    }
    fn usage(&self) -> (usize, usize) {
        self.inner.usage()
    }
}
fn limits(role: Role) -> Limits {
    Limits {
        streams: 2048,
        pending: 2048,
        fills: 1,
        qps: 1000,
        queue_timeout: Duration::from_millis(100),
        cooldown: Duration::from_millis(20),
        upstream_timeout: Duration::from_millis(if role == Role::Cache { 50 } else { 200 }),
        ..Default::default()
    }
}
fn region(channel: &Channel, store: Arc<WriteFault>) -> Arc<Layer> {
    Layer::new(
        Role::Cache,
        vec![REPO.to_vec()],
        "node-hop",
        "server-hop",
        Client::new(channel.clone(), Some("server-hop"), Some(Role::Server)).unwrap(),
        store,
        limits(Role::Cache),
    )
    .unwrap()
}
fn node(channel: &Channel, directory: &std::path::Path, n: usize) -> Arc<Layer> {
    Layer::new(
        Role::Daemon,
        vec![REPO.to_vec()],
        &format!("client-{n}"),
        "node-hop",
        Client::new(channel.clone(), Some("node-hop"), Some(Role::Cache)).unwrap(),
        Arc::new(DiskStore::open(directory).unwrap()),
        limits(Role::Daemon),
    )
    .unwrap()
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CacheEvent {
    pub phase: String,
    pub virtual_ms: u64,
    pub authority_calls: u64,
    pub cache_rpc_calls: u64,
    pub cache_pending: usize,
    pub node_pending: Vec<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CacheSimulation {
    pub seed: u64,
    pub clients: usize,
    pub virtual_time_ms: u64,
    pub initial_authority_requests: u64,
    pub initial_node_coalesced: Vec<u64>,
    pub initial_cache_coalesced: u64,
    pub outage_authority_requests: u64,
    pub truncated_replies: u64,
    pub timed_out_replies: u64,
    pub persistence_failures: u64,
    pub authority_requests: u64,
    pub disk_reopen_authority_requests: u64,
    pub events: Vec<CacheEvent>,
    pub result_digest: String,
}
async fn source_get(
    node: Arc<Layer>,
    key: Key,
    source: &Authority,
    healthy: bool,
) -> std::result::Result<Frame, Status> {
    tick(2).await;
    let calls = source.calls.load(Ordering::SeqCst);
    let sleeping = source.sleeping.load(Ordering::SeqCst);
    let task = tokio::spawn(async move { node.get(REPO, key).await });
    until(|| {
        source.calls.load(Ordering::SeqCst) == calls + 1
            && (!healthy || source.sleeping.load(Ordering::SeqCst) == sleeping + 1)
    })
    .await;
    if healthy {
        tick(5).await;
    }
    spin(task).await.unwrap()
}
fn event(
    phase: &str,
    start: Instant,
    source: &Authority,
    rpc: &CacheRpcState,
    nodes: &[Arc<Layer>],
) -> CacheEvent {
    let cache_pending = rpc
        .current
        .lock()
        .unwrap()
        .as_ref()
        .map_or(0, |cache| cache.stats().pending);
    let node_pending = nodes
        .iter()
        .map(|node| node.stats().pending)
        .collect::<Vec<_>>();
    assert_eq!(cache_pending, 0);
    assert!(node_pending.iter().all(|n| *n == 0));
    CacheEvent {
        phase: phase.into(),
        virtual_ms: start.elapsed().as_millis() as u64,
        authority_calls: source.calls.load(Ordering::SeqCst),
        cache_rpc_calls: rpc.calls.load(Ordering::SeqCst),
        cache_pending,
        node_pending,
    }
}

pub fn simulate_cache(seed: u64, clients: usize) -> CacheSimulation {
    assert!((2..=10_000).contains(&clients));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
        .block_on(run(seed, clients))
}
async fn run(seed: u64, clients: usize) -> CacheSimulation {
    let start = Instant::now();
    let directory = tempfile::tempdir().unwrap();
    let meta = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(FsBlobStore::open(directory.path().join("source")).unwrap());
    let repo = Repository {
        meta: meta.as_ref(),
        blobs: blobs.as_ref(),
    };
    let mut random = seed;
    let mut keys = Vec::new();
    let mut frames = Vec::new();
    for n in 0..10 {
        let raw: Vec<_> = (0..(128 * 1024 + n))
            .map(|_| {
                random = random
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (random >> 32) as u8
            })
            .collect();
        let id = repo.put_blob(&raw).unwrap();
        repo.associate_blob(REPO, id).unwrap();
        keys.push(Key {
            kind: Kind::Blob,
            id: ObjectId(id.0),
        });
        frames.push(Arc::new(blobs.get_compressed(ObjectId(id.0)).unwrap()));
    }
    let backend = Arc::new(
        Backend::new(REPO.to_vec(), meta.clone(), blobs.clone(), meta, None)
            .with_object_token("server-hop".into())
            .unwrap(),
    );
    let source = Arc::new(Authority {
        inner: ObjectService::new(backend),
        gate: Gate::held(),
        mode: AtomicU8::new(0),
        calls: AtomicU64::new(0),
        sleeping: AtomicU64::new(0),
        truncated: AtomicU64::new(0),
        stalled: AtomicU64::new(0),
    });
    let authority_peer = peer(AuthorityRpc(source.clone())).await;
    let store = Arc::new(WriteFault::new());
    let cache = region(&authority_peer.channel, store.clone());
    let rpc = Arc::new(CacheRpcState {
        current: Mutex::new(Some(cache.clone())),
        gate: Gate::default(),
        calls: AtomicU64::new(0),
    });
    let cache_peer = peer(CacheRpc(rpc.clone())).await;
    let node_paths: Vec<_> = (0..2)
        .map(|n| directory.path().join(format!("node-{n}")))
        .collect();
    let mut nodes: Vec<_> = node_paths
        .iter()
        .enumerate()
        .map(|(n, path)| node(&cache_peer.channel, path, n))
        .collect();
    let mut order: Vec<_> = (0..clients).collect();
    for n in (1..clients).rev() {
        random = random
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        order.swap(n, (random as usize) % (n + 1));
    }
    let schedule = order.clone();
    let mut joins = tokio::task::JoinSet::new();
    for n in order {
        let node = nodes[n % 2].clone();
        let key = keys[0];
        joins.spawn(async move { (n, node.get(REPO, key).await) });
    }
    until(|| {
        source.calls.load(Ordering::SeqCst) == 1
            && nodes[0].stats().requests == clients.div_ceil(2) as u64
            && nodes[1].stats().requests == (clients / 2) as u64
            && cache.stats().coalesced == 1
    })
    .await;
    source.gate.release();
    until(|| source.sleeping.load(Ordering::SeqCst) == 1).await;
    tick(5).await;
    let mut completed = vec![false; clients];
    while let Some(result) = spin(joins.join_next()).await {
        let (n, result) = result.unwrap();
        assert_eq!(result.unwrap(), frames[0]);
        assert!(!completed[n]);
        completed[n] = true;
    }
    assert!(completed.iter().all(|done| *done));
    let initial_node_coalesced = nodes
        .iter()
        .map(|n| n.stats().coalesced)
        .collect::<Vec<_>>();
    assert_eq!(
        initial_node_coalesced,
        vec![clients.div_ceil(2) as u64 - 1, clients as u64 / 2 - 1]
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    let mut events = vec![event("coalesced_wave", start, &source, &rpc, &nodes)];
    let initial_cache_coalesced = cache.stats().coalesced;

    *rpc.current.lock().unwrap() = None;
    for node in &nodes {
        assert_eq!(
            spin(node.get(REPO, keys[0])).await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
        tick(2).await;
        assert_eq!(
            spin(node.get(REPO, keys[1])).await.unwrap_err().code(),
            tonic::Code::Unavailable
        );
    }
    let outage_authority_requests = source.calls.load(Ordering::SeqCst) - 1;
    assert_eq!(outage_authority_requests, 0);
    events.push(event(
        "cache_unavailable_cached_and_cold_fail_closed",
        start,
        &source,
        &rpc,
        &nodes,
    ));
    drop(cache);
    drop(store);
    let store = Arc::new(WriteFault::new());
    assert!(store.get(keys[0]).unwrap().is_none());
    *rpc.current.lock().unwrap() = Some(region(&authority_peer.channel, store.clone()));
    assert_eq!(
        source_get(nodes[0].clone(), keys[1], &source, true)
            .await
            .unwrap(),
        frames[1]
    );
    tick(2).await;
    assert_eq!(spin(nodes[1].get(REPO, keys[1])).await.unwrap(), frames[1]);
    events.push(event("empty_cache_restarted", start, &source, &rpc, &nodes));

    source.mode.store(1, Ordering::SeqCst);
    assert_eq!(
        source_get(nodes[0].clone(), keys[2], &source, false)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    assert!(store.get(keys[2]).unwrap().is_none());
    events.push(event(
        "truncated_frame_rejected",
        start,
        &source,
        &rpc,
        &nodes,
    ));
    source.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        source_get(nodes[0].clone(), keys[2], &source, true)
            .await
            .unwrap(),
        frames[2]
    );

    source.mode.store(2, Ordering::SeqCst);
    tick(2).await;
    let stalled_before = source.stalled.load(Ordering::SeqCst);
    let node0 = nodes[0].clone();
    let key = keys[3];
    let stalled = tokio::spawn(async move { node0.get(REPO, key).await });
    until(|| source.stalled.load(Ordering::SeqCst) == stalled_before + 1).await;
    tick(51).await;
    assert_eq!(
        spin(stalled).await.unwrap().unwrap_err().code(),
        tonic::Code::DeadlineExceeded
    );
    assert!(store.get(keys[3]).unwrap().is_none());
    events.push(event("upstream_deadline", start, &source, &rpc, &nodes));
    source.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        source_get(nodes[0].clone(), keys[3], &source, true)
            .await
            .unwrap(),
        frames[3]
    );

    store.fail.store(true, Ordering::SeqCst);
    for key in keys.iter().skip(4).take(3).copied() {
        assert_eq!(
            source_get(nodes[0].clone(), key, &source, true)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Internal
        );
        assert!(store.get(key).unwrap().is_none());
    }
    let before = source.calls.load(Ordering::SeqCst);
    rpc.gate.hold.store(true, Ordering::SeqCst);
    tick(2).await;
    let requests = nodes[1].stats().requests;
    let calls = rpc.calls.load(Ordering::SeqCst);
    for _ in 0..30 {
        let node = nodes[1].clone();
        let key = keys[7];
        joins.spawn(async move { (0, node.get(REPO, key).await) });
    }
    until(|| {
        nodes[1].stats().requests == requests + 30 && rpc.calls.load(Ordering::SeqCst) == calls + 1
    })
    .await;
    rpc.gate.release();
    while let Some(result) = spin(joins.join_next()).await {
        assert_eq!(
            result.unwrap().1.unwrap_err().code(),
            tonic::Code::Unavailable
        );
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), before);
    assert_eq!(spin(nodes[0].get(REPO, keys[0])).await.unwrap(), frames[0]);
    events.push(event(
        "persistence_circuit_blocks_burst",
        start,
        &source,
        &rpc,
        &nodes,
    ));
    tick(21).await;
    assert_eq!(
        source_get(nodes[1].clone(), keys[7], &source, true)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Internal
    );
    tick(2).await;
    assert_eq!(
        spin(nodes[1].get(REPO, keys[8])).await.unwrap_err().code(),
        tonic::Code::Unavailable
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), before + 1);
    events.push(event(
        "failed_half_open_probe",
        start,
        &source,
        &rpc,
        &nodes,
    ));
    store.fail.store(false, Ordering::SeqCst);
    tick(21).await;
    assert_eq!(
        source_get(nodes[0].clone(), keys[7], &source, true)
            .await
            .unwrap(),
        frames[7]
    );
    tick(2).await;
    assert_eq!(spin(nodes[1].get(REPO, keys[7])).await.unwrap(), frames[7]);
    events.push(event("persistence_recovered", start, &source, &rpc, &nodes));
    let before_reopen = source.calls.load(Ordering::SeqCst);
    nodes[0] = node(&cache_peer.channel, &node_paths[0], 0);
    assert_eq!(spin(nodes[0].get(REPO, keys[0])).await.unwrap(), frames[0]);
    assert_eq!(spin(nodes[0].get(REPO, keys[7])).await.unwrap(), frames[7]);
    let disk_reopen_authority_requests = source.calls.load(Ordering::SeqCst) - before_reopen;
    assert_eq!(disk_reopen_authority_requests, 0);
    events.push(event("daemon_disk_reopened", start, &source, &rpc, &nodes));
    assert_eq!(source.calls.load(Ordering::SeqCst), 11);
    assert_eq!(store.failures.load(Ordering::SeqCst), 4);
    let mut digest = serde_json::to_vec(&(seed, clients, &schedule, &completed, &events)).unwrap();
    for key in &keys {
        digest.extend_from_slice(&key.id.0);
    }
    CacheSimulation {
        seed,
        clients,
        virtual_time_ms: start.elapsed().as_millis() as u64,
        initial_authority_requests: 1,
        initial_node_coalesced,
        initial_cache_coalesced,
        outage_authority_requests,
        truncated_replies: source.truncated.load(Ordering::SeqCst),
        timed_out_replies: source.stalled.load(Ordering::SeqCst),
        persistence_failures: store.failures.load(Ordering::SeqCst),
        authority_requests: source.calls.load(Ordering::SeqCst),
        disk_reopen_authority_requests,
        events,
        result_digest: ObjectId::hash(&digest).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seeded_cache_faults_repeat_with_real_layers_and_virtual_deadlines() {
        let mut digests = std::collections::BTreeSet::new();
        for seed in [0, 1, 42, 2026] {
            let first = simulate_cache(seed, 32);
            assert_eq!(first, simulate_cache(seed, 32));
            assert!(digests.insert(first.result_digest));
        }
        let different_size = simulate_cache(42, 33);
        assert!(!digests.contains(&different_size.result_digest));
    }
}
