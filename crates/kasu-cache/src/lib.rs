use kasu_core::ObjectId;
use kasu_object::{BATCH_LIMIT, CHUNK_BYTES, Client, Kind, LAYER_HEADER, Layer as Role};
use kasu_proto::v1::{
    self as p,
    objects_server::{Objects, ObjectsServer},
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Semaphore, watch},
    time::Instant,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{Instrument, instrument::WithSubscriber};
mod disk;
pub use disk::{DiskLimits, DiskStats, DiskStore};
pub mod runtime;

pub type Frame = Arc<Vec<u8>>;
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Key {
    pub kind: Kind,
    pub id: ObjectId,
}
/// Frames are shared by ID across repositories; a frame is served for a
/// repository only after the upstream confirmed that repository has it.
pub trait Store: Send + Sync {
    fn hot(&self, _key: Key) -> Result<Option<Frame>, Status> {
        Ok(None)
    }
    fn get(&self, key: Key) -> Result<Option<Frame>, Status>;
    fn put(&self, key: Key, bytes: Frame) -> Result<(), Status>;
    /// Whether the upstream confirmed that `repo` (a normalized name) has `key`.
    fn member(&self, repo: &[u8], key: Key) -> Result<bool, Status>;
    fn add_member(&self, repo: &[u8], key: Key) -> Result<(), Status>;
    /// Check membership and its repository identity atomically.
    fn member_scoped(&self, repo: &[u8], identity: &str, key: Key) -> Result<bool, Status>;
    /// Add membership only if the stored identity still matches the fetch.
    fn add_member_scoped(&self, repo: &[u8], identity: &str, key: Key) -> Result<(), Status>;
    /// Record the upstream identity of `repo`. A different identity means the
    /// name now belongs to another repository, so its memberships are dropped.
    fn identify(&self, repo: &[u8], identity: &str) -> Result<(), Status>;
    fn identity(&self, _repo: &[u8]) -> Result<Option<String>, Status> {
        Ok(None)
    }
    fn usage(&self) -> (usize, usize) {
        (0, 0)
    }
    fn disk_stats(&self) -> Option<DiskStats> {
        None
    }
}
struct Memory {
    entries: hashlink::LinkedHashMap<Key, Frame>,
    bytes: usize,
    members: hashlink::LinkedHashMap<(Vec<u8>, Key), ()>,
    identities: HashMap<Vec<u8>, String>,
}
const MAX_MEMORY_MEMBERS: usize = 1 << 20;
pub struct MemoryStore {
    inner: Mutex<Memory>,
    limit: usize,
}
impl MemoryStore {
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Mutex::new(Memory {
                entries: Default::default(),
                bytes: 0,
                members: Default::default(),
                identities: HashMap::new(),
            }),
            limit,
        }
    }
}
impl MemoryStore {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Memory>, Status> {
        self.inner
            .lock()
            .map_err(|_| Status::internal("memory cache lock failed"))
    }
    fn add_member_locked(state: &mut Memory, repo: &[u8], key: Key) {
        state.members.insert((repo.to_vec(), key), ());
        while state.members.len() > MAX_MEMORY_MEMBERS {
            state.members.pop_front();
        }
    }
}
impl Store for MemoryStore {
    fn member(&self, repo: &[u8], key: Key) -> Result<bool, Status> {
        Ok(self
            .lock()?
            .members
            .to_back(&(repo.to_vec(), key))
            .is_some())
    }
    fn add_member(&self, repo: &[u8], key: Key) -> Result<(), Status> {
        let mut state = self.lock()?;
        Self::add_member_locked(&mut state, repo, key);
        Ok(())
    }
    fn member_scoped(&self, repo: &[u8], identity: &str, key: Key) -> Result<bool, Status> {
        let mut state = self.lock()?;
        Ok(
            state.identities.get(repo).map(String::as_str) == Some(identity)
                && state.members.to_back(&(repo.to_vec(), key)).is_some(),
        )
    }
    fn add_member_scoped(&self, repo: &[u8], identity: &str, key: Key) -> Result<(), Status> {
        let mut state = self.lock()?;
        if state.identities.get(repo).map(String::as_str) != Some(identity) {
            return Err(Status::aborted(
                "repository identity changed during object fetch",
            ));
        }
        Self::add_member_locked(&mut state, repo, key);
        Ok(())
    }
    fn identify(&self, repo: &[u8], identity: &str) -> Result<(), Status> {
        let mut state = self.lock()?;
        if state
            .identities
            .insert(repo.to_vec(), identity.to_owned())
            .is_some_and(|old| old != identity)
        {
            state.members.retain(|(name, _), _| name != repo);
        }
        Ok(())
    }
    fn identity(&self, repo: &[u8]) -> Result<Option<String>, Status> {
        Ok(self.lock()?.identities.get(repo).cloned())
    }
    fn hot(&self, key: Key) -> Result<Option<Frame>, Status> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| Status::internal("memory cache lock failed"))?;
        Ok(state.entries.to_back(&key).cloned())
    }
    fn get(&self, key: Key) -> Result<Option<Frame>, Status> {
        self.hot(key)
    }
    fn put(&self, key: Key, bytes: Frame) -> Result<(), Status> {
        kasu_object::validate(key.kind, key.id, &bytes)?;
        if bytes.len() > self.limit {
            return Ok(());
        }
        let mut state = self
            .inner
            .lock()
            .map_err(|_| Status::internal("memory cache lock failed"))?;
        if state.entries.contains_key(&key) {
            return Ok(());
        }
        while state.bytes + bytes.len() > self.limit {
            if let Some((_, old)) = state.entries.pop_front() {
                state.bytes -= old.len();
            } else {
                break;
            }
        }
        state.bytes += bytes.len();
        state.entries.insert(key, bytes);
        Ok(())
    }
    fn usage(&self) -> (usize, usize) {
        self.inner
            .lock()
            .map(|s| (s.entries.len(), s.bytes))
            .unwrap_or_default()
    }
}
#[derive(Clone, Debug)]
pub struct Limits {
    pub streams: usize,
    pub fills: usize,
    pub pending: usize,
    pub qps: u32,
    pub queue_timeout: Duration,
    pub upstream_timeout: Duration,
    pub cooldown: Duration,
    pub failure_threshold: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            streams: 16,
            fills: 2,
            pending: 128,
            qps: 20,
            queue_timeout: Duration::from_secs(1),
            upstream_timeout: Duration::from_secs(5),
            cooldown: Duration::from_secs(1),
            failure_threshold: 3,
        }
    }
}
impl Limits {
    fn validate(&self) -> Result<(), Status> {
        if !(1..=4096).contains(&self.streams)
            || !(1..=128).contains(&self.fills)
            || !(1..=4096).contains(&self.pending)
            || self.qps == 0
            || self.qps > 100_000
            || self.failure_threshold == 0
            || self.queue_timeout.is_zero()
            || self.upstream_timeout.is_zero()
            || self.cooldown.is_zero()
            || self.queue_timeout > Duration::from_secs(30)
            || self.upstream_timeout > Duration::from_secs(30)
            || self.cooldown > Duration::from_secs(300)
        {
            return Err(Status::invalid_argument("invalid object layer limits"));
        }
        Ok(())
    }
}
#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced: AtomicU64,
    rejected: AtomicU64,
    upstream: AtomicU64,
    errors: AtomicU64,
    active: AtomicU64,
    max_active: AtomicU64,
    circuit_rejections: AtomicU64,
}
#[derive(Debug, serde::Serialize)]
pub struct Stats {
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub coalesced: u64,
    pub rejected: u64,
    pub upstream: u64,
    pub errors: u64,
    pub active: u64,
    pub max_active: u64,
    pub circuit_rejections: u64,
    pub pending: usize,
    pub entries: usize,
    pub bytes: usize,
}
#[derive(Default)]
struct Breaker {
    failures: u32,
    until: Option<Instant>,
    probe: bool,
}
impl Breaker {
    fn admit(&mut self, threshold: u32) -> Result<(), Status> {
        if self.failures >= threshold {
            if self.probe || self.until.is_some_and(|until| until > Instant::now()) {
                return Err(Status::unavailable("upstream circuit is cooling down"));
            }
            self.probe = true;
        }
        Ok(())
    }
    fn finish<T>(&mut self, result: &Result<T, Status>, limits: &Limits) {
        self.probe = false;
        if result.is_ok()
            || result
                .as_ref()
                .is_err_and(|e| e.code() == tonic::Code::NotFound)
        {
            self.failures = 0;
            self.until = None;
        } else {
            self.failures = self.failures.saturating_add(1);
            if self.failures >= limits.failure_threshold {
                self.until = Some(Instant::now() + limits.cooldown);
            }
        }
    }
}
#[derive(Clone)]
struct Value {
    bytes: Frame,
    hit: bool,
}
type FlightResult = Result<Value, Status>;
struct Flight {
    tx: watch::Sender<Option<FlightResult>>,
}
pub struct Layer {
    role: Role,
    /// Normalized names this layer serves; empty serves every repository.
    repositories: Vec<Vec<u8>>,
    token: tonic::metadata::MetadataValue<tonic::metadata::Ascii>,
    upstream: Client,
    store: Arc<dyn Store>,
    limits: Limits,
    requests: Arc<Semaphore>,
    work: Arc<Semaphore>,
    flights: Mutex<HashMap<FlightKey, Arc<Flight>>>,
    rate: tokio::sync::Mutex<tokio::time::Instant>,
    breaker: Mutex<Breaker>,
    counters: Counters,
}
/// Names can be reused while an older repository's fetch is still in flight.
type FlightKey = (Vec<u8>, String, Key);
struct FlightGuard {
    layer: Arc<Layer>,
    key: FlightKey,
    flight: Arc<Flight>,
    done: bool,
}
impl FlightGuard {
    fn remove(&self) {
        if let Ok(mut flights) = self.layer.flights.lock()
            && flights
                .get(&self.key)
                .is_some_and(|f| Arc::ptr_eq(f, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
    fn finish(mut self, value: FlightResult) {
        self.remove();
        self.done = true;
        self.flight.tx.send_replace(Some(value));
    }
}
impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.remove();
        if !self.done {
            self.flight
                .tx
                .send_replace(Some(Err(Status::internal("object fill worker stopped"))));
        }
    }
}
struct Active<'a>(&'a AtomicU64);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
impl Layer {
    pub fn new(
        role: Role,
        repositories: Vec<Vec<u8>>,
        token: &str,
        upstream_token: &str,
        upstream: Client,
        store: Arc<dyn Store>,
        limits: Limits,
    ) -> Result<Arc<Self>, Status> {
        limits.validate()?;
        if repositories.len() > 4096 {
            return Err(Status::invalid_argument("too many layer repositories"));
        }
        let repositories = repositories
            .iter()
            .map(|r| normalize(r).ok_or_else(|| Status::invalid_argument("invalid repository")))
            .collect::<Result<Vec<_>, _>>()?;
        let expected = match role {
            Role::Cache => Role::Server,
            Role::Daemon => Role::Cache,
            Role::Server => return Err(Status::invalid_argument("server is not a cache layer")),
        };
        if upstream.expected_layer() != Some(expected) || !upstream.uses_token(upstream_token) {
            return Err(Status::invalid_argument(
                "upstream role or hop credential mismatch",
            ));
        }
        if role == Role::Server || token == upstream_token {
            return Err(Status::invalid_argument(
                "invalid layer role or identical hop credentials",
            ));
        }
        let token = kasu_object::bearer(token)?;
        kasu_object::bearer(upstream_token)?;
        Ok(Arc::new(Self {
            role,
            repositories,
            token,
            upstream,
            store,
            requests: Arc::new(Semaphore::new(limits.streams)),
            work: Arc::new(Semaphore::new(limits.fills)),
            limits,
            flights: Mutex::new(HashMap::new()),
            rate: tokio::sync::Mutex::new(tokio::time::Instant::now()),
            breaker: Mutex::new(Breaker::default()),
            counters: Counters::default(),
        }))
    }
    pub fn stats(&self) -> Stats {
        let c = &self.counters;
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        let (entries, bytes) = self.store.usage();
        Stats {
            requests: load(&c.requests),
            hits: load(&c.hits),
            misses: load(&c.misses),
            coalesced: load(&c.coalesced),
            rejected: load(&c.rejected),
            upstream: load(&c.upstream),
            errors: load(&c.errors),
            active: load(&c.active),
            max_active: load(&c.max_active),
            circuit_rejections: load(&c.circuit_rejections),
            pending: self.flights.lock().map(|s| s.len()).unwrap_or_default(),
            entries,
            bytes,
        }
    }
    pub fn metrics(&self) -> String {
        let s = self.stats();
        let mut text = String::new();
        for (name, value) in [
            ("requests_total", s.requests),
            ("hits_total", s.hits),
            ("misses_total", s.misses),
            ("coalesced_total", s.coalesced),
            ("rejected_total", s.rejected),
            ("upstream_requests_total", s.upstream),
            ("errors_total", s.errors),
            ("upstream_active", s.active),
            ("upstream_max_active", s.max_active),
            ("circuit_rejections_total", s.circuit_rejections),
            ("pending", s.pending as u64),
            ("entries", s.entries as u64),
            ("bytes", s.bytes as u64),
        ] {
            text.push_str(&format!(
                "kasu_object_{name}{{layer=\"{}\"}} {value}\n",
                self.role.name()
            ));
        }
        if let Some(disk) = self.store.disk_stats() {
            for (name, value) in [
                ("disk_limit_bytes", disk.limits.bytes),
                ("disk_limit_entries", disk.limits.entries),
                ("disk_evictions_total", disk.evictions),
                ("disk_evicted_bytes_total", disk.evicted_bytes),
                ("disk_admission_skips_total", disk.admission_skips),
                ("disk_repairs_total", disk.repairs),
            ] {
                text.push_str(&format!(
                    "kasu_object_{name}{{layer=\"{}\"}} {value}\n",
                    self.role.name()
                ));
            }
        }
        text
    }
    pub fn serves(&self, repo: &[u8]) -> bool {
        self.repositories.is_empty() || self.repositories.iter().any(|r| r == repo)
    }
    /// `repo` is a normalized repository name.
    pub async fn get(self: &Arc<Self>, repo: &[u8], key: Key) -> Result<Frame, Status> {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let identity = self.validate_repository(repo).await?;
        self.get_validated(repo, &identity, key).await
    }
    async fn validate_repository(&self, repo: &[u8]) -> Result<String, Status> {
        let identity = tokio::time::timeout(
            self.limits.upstream_timeout,
            self.upstream.repository_identity(repo),
        )
        .await
        .map_err(|_| Status::deadline_exceeded("repository identity upstream timed out"))??;
        let store = self.store.clone();
        let repo = repo.to_vec();
        let identity_for_store = identity.clone();
        kasu_telemetry::blocking(move || store.identify(&repo, &identity_for_store))
            .await
            .map_err(|_| Status::internal("object cache identity worker failed"))??;
        Ok(identity)
    }
    async fn get_validated(
        self: &Arc<Self>,
        repo: &[u8],
        identity: &str,
        key: Key,
    ) -> Result<Frame, Status> {
        if self.store.member_scoped(repo, identity, key)?
            && let Some(bytes) = self.store.hot(key)?
        {
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(bytes);
        }
        let flight_key = (repo.to_vec(), identity.to_owned(), key);
        let (flight, spawn) = {
            let mut flights = self
                .flights
                .lock()
                .map_err(|_| Status::internal("flight lock failed"))?;
            if let Some(flight) = flights.get(&flight_key) {
                self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
                (flight.clone(), false)
            } else {
                if flights.len() >= self.limits.pending {
                    self.counters.rejected.fetch_add(1, Ordering::Relaxed);
                    return Err(Status::resource_exhausted("object fill queue is full"));
                }
                let (tx, _) = watch::channel(None);
                let flight = Arc::new(Flight { tx });
                flights.insert(flight_key.clone(), flight.clone());
                (flight, true)
            }
        };
        let mut rx = flight.tx.subscribe();
        if spawn {
            let layer = self.clone();
            let guard = FlightGuard {
                layer: layer.clone(),
                key: flight_key.clone(),
                flight,
                done: false,
            };
            let repo = repo.to_vec();
            let identity = identity.to_owned();
            tokio::spawn(
                async move {
                    let result = layer.fill(&repo, &identity, key).await;
                    if result.is_err() {
                        layer.counters.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    kasu_telemetry::finish(
                        &tracing::Span::current(),
                        result.as_ref().err().map_or(tonic::Code::Ok, Status::code),
                    );
                    guard.finish(result);
                }
                .instrument(kasu_telemetry::operation_span("object_fill"))
                .with_current_subscriber(),
            );
        }
        let value = tokio::time::timeout(
            self.limits.queue_timeout + self.limits.upstream_timeout + Duration::from_secs(30),
            async {
                loop {
                    if let Some(result) = rx.borrow().clone() {
                        break result;
                    }
                    rx.changed()
                        .await
                        .map_err(|_| Status::internal("object fill ended without result"))?;
                }
            },
        )
        .await
        .map_err(|_| Status::deadline_exceeded("object layer wait expired"))??;
        if value.hit {
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.misses.fetch_add(1, Ordering::Relaxed);
        }
        Ok(value.bytes)
    }
    async fn fill(&self, repo: &[u8], identity: &str, key: Key) -> FlightResult {
        let _permit =
            tokio::time::timeout(self.limits.queue_timeout, self.work.clone().acquire_owned())
                .await
                .map_err(|_| {
                    self.counters.rejected.fetch_add(1, Ordering::Relaxed);
                    Status::resource_exhausted("object fill concurrency queue expired")
                })?
                .map_err(|_| Status::unavailable("object layer stopping"))?;
        let store = self.store.clone();
        let member = repo.to_vec();
        let expected_identity = identity.to_owned();
        let cached = kasu_telemetry::blocking(move || {
            if store.member_scoped(&member, &expected_identity, key)? {
                store.get(key)
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(|_| Status::internal("object cache read worker failed"))??;
        if let Some(bytes) = cached {
            return Ok(Value { bytes, hit: true });
        }
        // Holding the pacing mutex while sleeping avoids accumulating stale
        // reservations when queued clients time out.
        let mut next = self.rate.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = tokio::time::Instant::now()
            + Duration::from_nanos((1_000_000_000 / self.limits.qps as u64).max(1));
        drop(next);
        if let Err(error) = self
            .breaker
            .lock()
            .map_err(|_| Status::internal("breaker lock failed"))?
            .admit(self.limits.failure_threshold)
        {
            self.counters
                .circuit_rejections
                .fetch_add(1, Ordering::Relaxed);
            return Err(error);
        }
        self.counters.upstream.fetch_add(1, Ordering::Relaxed);
        let active = self.counters.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.counters
            .max_active
            .fetch_max(active, Ordering::Relaxed);
        let _active = Active(&self.counters.active);
        let result = tokio::time::timeout(
            self.limits.upstream_timeout,
            self.upstream.fetch_scoped(repo, key.id, key.kind),
        )
        .await
        .unwrap_or_else(|_| Err(Status::deadline_exceeded("object upstream timed out")));
        // A fill succeeds only when the validated frame can be kept by this
        // layer. Otherwise a full or unavailable disk would repeatedly fetch
        // the same cold objects despite every client request failing.
        let result = async {
            let (bytes, fetched_identity) = result?;
            if fetched_identity.as_deref() != Some(identity) {
                return Err(Status::aborted(
                    "repository identity changed during object fetch",
                ));
            }
            let bytes = Arc::new(bytes);
            let store = self.store.clone();
            let value = bytes.clone();
            let member = repo.to_vec();
            let identity = identity.to_owned();
            kasu_telemetry::blocking(move || {
                store.put(key, value)?;
                store.add_member_scoped(&member, &identity, key)
            })
            .await
            .map_err(|_| Status::internal("object cache write worker failed"))??;
            Ok(Value { bytes, hit: false })
        }
        .await;
        self.breaker
            .lock()
            .map_err(|_| Status::internal("breaker lock failed"))?
            .finish(&result, &self.limits);
        result
    }
}
/// The lowercase form used for membership; None for invalid names.
pub fn normalize(repo: &[u8]) -> Option<Vec<u8>> {
    kasu_core::RepoName::from_bytes(repo)
        .ok()
        .map(|name| name.normalized().into_bytes())
}
#[derive(Clone)]
pub struct Service {
    layer: Arc<Layer>,
}
impl Service {
    pub fn new(layer: Arc<Layer>) -> Self {
        Self { layer }
    }
    pub fn into_server(self) -> kasu_telemetry::GrpcService<ObjectsServer<Self>> {
        use tower::Layer;
        let inner = ObjectsServer::new(self)
            .max_decoding_message_size(16 * 1024)
            .max_encoding_message_size(CHUNK_BYTES + 1024);
        kasu_telemetry::GrpcLayer.layer(inner)
    }
    fn permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Status> {
        self.layer
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                self.layer.counters.rejected.fetch_add(1, Ordering::Relaxed);
                Status::resource_exhausted("object stream concurrency limit")
            })
    }
    async fn stream<T: Send + 'static>(
        &self,
        q: Request<p::ObjectRequest>,
        kind: Kind,
        encode: fn(Vec<u8>, u64, Vec<u8>, bool) -> T,
    ) -> Result<Response<ReceiverStream<Result<T, Status>>>, Status> {
        if !kasu_object::authorized(&self.layer.token, q.metadata().get("authorization")) {
            return Err(Status::unauthenticated("object layer token required"));
        }
        let input = q.into_inner();
        // Other repositories look exactly like ones that do not exist.
        let repo = normalize(&input.repo)
            .filter(|repo| self.layer.serves(repo))
            .ok_or_else(|| Status::not_found("repository not found"))?;
        if input.ids.len() > BATCH_LIMIT {
            return Err(Status::resource_exhausted("object batch exceeds limit"));
        }
        let ids = input
            .ids
            .iter()
            .map(|id| {
                id.as_slice()
                    .try_into()
                    .map(ObjectId)
                    .map_err(|_| Status::invalid_argument("object ID must be 32 bytes"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if ids.iter().copied().collect::<BTreeSet<_>>().len() != ids.len() {
            return Err(Status::invalid_argument("duplicate object ID"));
        }
        let permit = self.permit()?;
        let identity = self.layer.validate_repository(&repo).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let layer = self.layer.clone();
        let stream_identity = identity.clone();
        tokio::spawn(
            async move {
                let _permit = permit;
                for id in ids {
                    let result =
                        tokio::select! {_=tx.closed()=>return,r=layer.get_validated(&repo,&stream_identity,Key{kind,id})=>r};
                    let bytes = match result {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    };
                    for (index, data) in bytes.chunks(CHUNK_BYTES).enumerate() {
                        let offset = index * CHUNK_BYTES;
                        if tx
                            .send(Ok(encode(
                                id.0.to_vec(),
                                offset as u64,
                                data.to_vec(),
                                offset + data.len() == bytes.len(),
                            )))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            .in_current_span()
            .with_current_subscriber(),
        );
        let mut response = Response::new(ReceiverStream::new(rx));
        response
            .metadata_mut()
            .insert(LAYER_HEADER, self.layer.role.name().parse().unwrap());
        if let Ok(value) = identity.parse() {
            response
                .metadata_mut()
                .insert(kasu_object::REPOSITORY_HEADER, value);
        }
        Ok(response)
    }
}
#[tonic::async_trait]
impl Objects for Service {
    type GetTreesStream = ReceiverStream<Result<p::TreeObject, Status>>;
    type GetBlobsStream = ReceiverStream<Result<p::BlobChunk, Status>>;
    async fn get_trees(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> Result<Response<Self::GetTreesStream>, Status> {
        self.stream(q, Kind::Tree, |id, offset, data, last| p::TreeObject {
            id,
            offset,
            data,
            last,
        })
        .await
    }
    async fn get_blobs(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> Result<Response<Self::GetBlobsStream>, Status> {
        self.stream(q, Kind::Blob, |id, offset, data, last| p::BlobChunk {
            id,
            offset,
            data,
            last,
        })
        .await
    }
    async fn get_repository_identity(
        &self,
        q: Request<p::RepositoryIdentityRequest>,
    ) -> Result<Response<p::RepositoryIdentityResponse>, Status> {
        if !kasu_object::authorized(&self.layer.token, q.metadata().get("authorization")) {
            return Err(Status::unauthenticated("object layer token required"));
        }
        let repo = normalize(&q.into_inner().repo)
            .filter(|repo| self.layer.serves(repo))
            .ok_or_else(|| Status::not_found("repository not found"))?;
        let _permit = self.permit()?;
        let identity = self.layer.validate_repository(&repo).await?;
        let mut response = Response::new(p::RepositoryIdentityResponse { identity });
        response
            .metadata_mut()
            .insert(LAYER_HEADER, self.layer.role.name().parse().unwrap());
        Ok(response)
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn circuit_cooldown_and_half_open_probe_follow_the_runtime_clock() {
        let limits = Limits {
            failure_threshold: 1,
            cooldown: Duration::from_millis(100),
            ..Default::default()
        };
        let mut breaker = Breaker::default();
        breaker.finish::<()>(&Err(Status::unavailable("injected outage")), &limits);
        assert!(breaker.admit(limits.failure_threshold).is_err());
        tokio::time::advance(limits.cooldown).await;
        assert!(breaker.admit(limits.failure_threshold).is_ok());
        assert!(breaker.admit(limits.failure_threshold).is_err());
        breaker.finish(&Ok(()), &limits);
        assert!(breaker.admit(limits.failure_threshold).is_ok());
    }
}
