use crate::Backend;
use axum::{
    body::{Body, Bytes},
    http::{Request, Response},
    response::IntoResponse,
};
use http_body::{Body as HttpBody, Frame, SizeHint};
use kasu_core::RepoKey;
use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::Status;
use tower::{Layer, Service};
use tracing::{Instrument, instrument::WithSubscriber};

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerLimits {
    pub active: usize,
    pub per_identity_active: usize,
    pub pending: usize,
    pub queue_timeout_ms: u64,
    pub client_rate_per_second: u32,
    pub client_burst: u32,
    pub token_rate_per_second: u32,
    pub token_burst: u32,
    pub max_clients: usize,
    pub idle_ms: u64,
    /// Concurrent requests one repository may run (default: half of
    /// `active`); capped per repository by its own configuration. Requests
    /// queued on a busy repository release their server-wide slot meanwhile.
    pub per_repository_active: Option<usize>,
    /// Requests one repository may queue (default: a quarter of `pending`).
    pub per_repository_pending: Option<usize>,
    /// Concurrent requests of one delegate across every account it acts as.
    pub per_delegate_active: Option<usize>,
}
impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            active: 64,
            per_identity_active: 16,
            pending: 1024,
            queue_timeout_ms: 1000,
            client_rate_per_second: 10000,
            client_burst: 20000,
            token_rate_per_second: 1000,
            token_burst: 2000,
            max_clients: 4096,
            idle_ms: 60000,
            per_repository_active: None,
            per_repository_pending: None,
            per_delegate_active: None,
        }
    }
}
impl ServerLimits {
    pub fn validate(&self) -> Result<(), Status> {
        if !(1..=4096).contains(&self.active)
            || !(1..=self.active).contains(&self.per_identity_active)
            || !(1..=65536).contains(&self.pending)
            || !(1..=30000).contains(&self.queue_timeout_ms)
            || !(1..=65536).contains(&self.max_clients)
            || !(1000..=3600000).contains(&self.idle_ms)
            || !(1..=self.active).contains(&self.repository_active())
            || !(1..=self.pending).contains(&self.repository_pending())
            || !(1..=self.active).contains(&self.delegate_active())
            || [
                self.client_rate_per_second,
                self.client_burst,
                self.token_rate_per_second,
                self.token_burst,
            ]
            .iter()
            .any(|n| !(1..=1_000_000).contains(n))
        {
            return Err(Status::invalid_argument("invalid server admission limits"));
        }
        Ok(())
    }
    pub fn repository_active(&self) -> usize {
        self.per_repository_active
            .unwrap_or_else(|| (self.active / 2).max(1))
    }
    pub fn repository_pending(&self) -> usize {
        self.per_repository_pending
            .unwrap_or_else(|| (self.pending / 4).max(1))
    }
    pub fn delegate_active(&self) -> usize {
        self.per_delegate_active.unwrap_or(self.active)
    }
    pub fn load(path: &std::path::Path) -> Result<Self, Status> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(16385).read_to_end(&mut bytes))
            .map_err(|_| Status::invalid_argument("cannot read server limits"))?;
        if bytes.len() > 16384 {
            return Err(Status::invalid_argument("server limits exceed 16 KiB"));
        }
        let value: Self = serde_json::from_slice(&bytes)
            .map_err(|_| Status::invalid_argument("invalid server limits configuration"))?;
        value.validate()?;
        Ok(value)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum Identity {
    Client(Option<IpAddr>),
    Operator,
    Projection(u16),
    Account(u16),
    Hop,
}
struct Bucket {
    tokens: f64,
    updated: Instant,
    seen: Instant,
    slots: Arc<Semaphore>,
}
pub(crate) struct Runtime {
    pub limits: ServerLimits,
    active: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    scrapes: Arc<Semaphore>,
    buckets: Mutex<BTreeMap<Identity, Bucket>>,
    repositories: Mutex<HashMap<RepoKey, Arc<RepositorySlots>>>,
    delegates: Mutex<HashMap<u16, Arc<Semaphore>>>,
    /// Normalized names exported as metric labels; others aggregate as "other".
    pub metric_repositories: std::collections::BTreeSet<String>,
    repository_counters: Mutex<BTreeMap<String, [u64; 2]>>,
    pub stats: Arc<ServerStats>,
}
struct RepositorySlots {
    slots: Arc<Semaphore>,
    limit: usize,
    waiting: AtomicUsize,
    label: String,
}
const MAX_REPOSITORY_SLOTS: usize = 65_536;
impl Runtime {
    pub fn new(limits: ServerLimits, stats: Arc<ServerStats>) -> Arc<Self> {
        Self::with_metrics(limits, stats, Default::default())
    }
    pub fn with_metrics(
        limits: ServerLimits,
        stats: Arc<ServerStats>,
        metric_repositories: std::collections::BTreeSet<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            active: Arc::new(Semaphore::new(limits.active)),
            pending: Arc::new(Semaphore::new(limits.pending)),
            scrapes: Arc::new(Semaphore::new(2)),
            limits,
            buckets: Mutex::new(BTreeMap::new()),
            repositories: Mutex::new(HashMap::new()),
            delegates: Mutex::new(HashMap::new()),
            metric_repositories,
            repository_counters: Mutex::new(BTreeMap::new()),
            stats,
        })
    }
    fn count_repository(&self, label: &str, rejected: bool) {
        if let Ok(mut counters) = self.repository_counters.lock() {
            counters.entry(label.to_owned()).or_default()[usize::from(rejected)] += 1;
        }
    }
    fn repository_slots(
        &self,
        key: &RepoKey,
        label: String,
        limit: usize,
    ) -> Result<Arc<RepositorySlots>, Status> {
        let mut repositories = self
            .repositories
            .lock()
            .map_err(|_| Status::internal("admission lock failed"))?;
        if let Some(slots) = repositories.get(key)
            && slots.limit == limit
            && slots.label == label
        {
            return Ok(slots.clone());
        }
        if repositories.len() >= MAX_REPOSITORY_SLOTS {
            repositories.retain(|_, s| Arc::strong_count(s) > 1 || Arc::strong_count(&s.slots) > 1);
        }
        // A changed limit takes effect for new requests; running ones keep
        // the permits of the previous semaphore until they finish.
        let slots = Arc::new(RepositorySlots {
            slots: Arc::new(Semaphore::new(limit)),
            limit,
            waiting: AtomicUsize::new(0),
            label,
        });
        repositories.insert(key.clone(), slots.clone());
        Ok(slots)
    }
    /// Take a slot of `key` for the current request. Queued requests release
    /// their server-wide slot while waiting, so one busy repository cannot
    /// occupy the whole server.
    pub(crate) async fn admit_repository(
        &self,
        key: &RepoKey,
        name: &kasu_core::RepoName,
        limits: &crate::registry::RepositoryLimits,
    ) -> Result<(), Status> {
        let Ok(lease) = LEASE.try_with(Clone::clone) else {
            return Ok(());
        };
        if lease
            .0
            .repository
            .lock()
            .map_err(|_| Status::internal("admission lock failed"))?
            .is_some()
        {
            return Ok(());
        }
        let normalized = name.normalized();
        let label = if self.metric_repositories.contains(&normalized) {
            normalized
        } else {
            "other".to_owned()
        };
        let active = limits
            .active
            .unwrap_or(usize::MAX)
            .min(self.limits.repository_active());
        let pending = limits
            .pending
            .unwrap_or(usize::MAX)
            .min(self.limits.repository_pending());
        let slots = self.repository_slots(key, label.clone(), active)?;
        let permit = match slots.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if slots.waiting.fetch_add(1, Ordering::AcqRel) >= pending {
                    slots.waiting.fetch_sub(1, Ordering::AcqRel);
                    self.count_repository(&label, true);
                    return Err(Status::resource_exhausted(
                        "repository request queue is full",
                    ));
                }
                // Dropping the permit here is what frees the server-wide slot.
                let released = lease
                    .0
                    .active
                    .lock()
                    .map_err(|_| Status::internal("admission lock failed"))?
                    .take()
                    .is_some();
                let timeout = Duration::from_millis(self.limits.queue_timeout_ms);
                let waited =
                    tokio::time::timeout(timeout, slots.slots.clone().acquire_owned()).await;
                slots.waiting.fetch_sub(1, Ordering::AcqRel);
                let permit = match waited {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => return Err(Status::unavailable("admission closed")),
                    Err(_) => {
                        self.count_repository(&label, true);
                        return Err(Status::resource_exhausted(
                            "repository queue deadline exceeded",
                        ));
                    }
                };
                if released {
                    let active = tokio::time::timeout(timeout, self.active.clone().acquire_owned())
                        .await
                        .map_err(|_| Status::resource_exhausted("request queue deadline exceeded"))?
                        .map_err(|_| Status::unavailable("admission closed"))?;
                    *lease
                        .0
                        .active
                        .lock()
                        .map_err(|_| Status::internal("admission lock failed"))? = Some(active);
                }
                permit
            }
        };
        self.count_repository(&label, false);
        *lease
            .0
            .repository
            .lock()
            .map_err(|_| Status::internal("admission lock failed"))? = Some(permit);
        Ok(())
    }
    /// A slot of `delegate`'s concurrency limit, shared by every account it
    /// acts as.
    fn delegate_slot(&self, delegate: u16) -> Result<OwnedSemaphorePermit, Status> {
        let slots = self
            .delegates
            .lock()
            .map_err(|_| Status::internal("admission lock failed"))?
            .entry(delegate)
            .or_insert_with(|| Arc::new(Semaphore::new(self.limits.delegate_active())))
            .clone();
        slots
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("delegate concurrency limit exceeded"))
    }
    fn bucket(&self, key: Identity, now: Instant) -> Result<Arc<Semaphore>, Status> {
        let client = matches!(key, Identity::Client(_));
        let (rate, burst) = if client {
            (self.limits.client_rate_per_second, self.limits.client_burst)
        } else {
            (self.limits.token_rate_per_second, self.limits.token_burst)
        };
        let mut buckets = self
            .buckets
            .lock()
            .map_err(|_| Status::internal("admission lock failed"))?;
        if !buckets.contains_key(&key) {
            buckets.retain(|identity, b| {
                let (rate, burst) = if matches!(identity, Identity::Client(_)) {
                    (self.limits.client_rate_per_second, self.limits.client_burst)
                } else {
                    (self.limits.token_rate_per_second, self.limits.token_burst)
                };
                let refilled = b.tokens
                    + now.saturating_duration_since(b.updated).as_secs_f64() * f64::from(rate);
                now.saturating_duration_since(b.seen) < Duration::from_millis(self.limits.idle_ms)
                    || Arc::strong_count(&b.slots) > 1
                    || refilled < f64::from(burst)
            });
            if client
                && buckets
                    .keys()
                    .filter(|k| matches!(k, Identity::Client(_)))
                    .count()
                    >= self.limits.max_clients
            {
                return Err(Status::resource_exhausted("client admission table is full"));
            }
            buckets.insert(
                key,
                Bucket {
                    tokens: f64::from(burst),
                    updated: now,
                    seen: now,
                    slots: Arc::new(Semaphore::new(self.limits.per_identity_active)),
                },
            );
        }
        let b = buckets.get_mut(&key).unwrap();
        b.tokens = (b.tokens
            + now.saturating_duration_since(b.updated).as_secs_f64() * f64::from(rate))
        .min(f64::from(burst));
        b.updated = now;
        b.seen = now;
        if b.tokens < 1.0 {
            return Err(Status::resource_exhausted("request rate limit exceeded"));
        }
        b.tokens -= 1.0;
        Ok(b.slots.clone())
    }
    async fn acquire(
        self: &Arc<Self>,
        slots: Arc<Semaphore>,
        attempt: Arc<Attempt>,
    ) -> Result<RequestLease, Status> {
        let pending = self
            .pending
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("request queue is full"))?;
        let acquire = async {
            let identity = slots
                .acquire_owned()
                .await
                .map_err(|_| Status::unavailable("admission closed"))?;
            let active = self
                .active
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Status::unavailable("admission closed"))?;
            Ok::<_, Status>((identity, active))
        };
        let (identity, active) =
            tokio::time::timeout(Duration::from_millis(self.limits.queue_timeout_ms), acquire)
                .await
                .map_err(|_| Status::resource_exhausted("request queue deadline exceeded"))??;
        drop(pending);
        attempt.admitted.store(1, Ordering::Relaxed);
        attempt.stats.active.fetch_add(1, Ordering::Relaxed);
        Ok(RequestLease(Arc::new(LeaseInner {
            _identity: Some(identity),
            _delegate: None,
            active: Mutex::new(Some(active)),
            repository: Mutex::new(None),
            attempt,
        })))
    }
    fn scrape(&self, attempt: Arc<Attempt>) -> Result<RequestLease, Status> {
        let permit = self
            .scrapes
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("metrics concurrency limit exceeded"))?;
        attempt.admitted.store(1, Ordering::Relaxed);
        attempt.stats.active.fetch_add(1, Ordering::Relaxed);
        Ok(RequestLease(Arc::new(LeaseInner {
            _identity: None,
            _delegate: None,
            active: Mutex::new(Some(permit)),
            repository: Mutex::new(None),
            attempt,
        })))
    }
    pub fn metrics(&self) -> String {
        let mut text = self.stats.render();
        text.push_str(concat!(
            "# TYPE kasu_server_repository_requests_total counter\n",
            "# TYPE kasu_server_repository_rejected_total counter\n",
            "# TYPE kasu_server_repository_active gauge\n",
        ));
        let mut active = BTreeMap::<String, usize>::new();
        if let Ok(repositories) = self.repositories.lock() {
            for slots in repositories.values() {
                *active.entry(slots.label.clone()).or_default() +=
                    slots.limit - slots.slots.available_permits();
            }
        }
        if let Ok(counters) = self.repository_counters.lock() {
            for (label, [admitted, rejected]) in counters.iter() {
                text.push_str(&format!(
                    "kasu_server_repository_requests_total{{repository=\"{label}\"}} {}\nkasu_server_repository_rejected_total{{repository=\"{label}\"}} {rejected}\nkasu_server_repository_active{{repository=\"{label}\"}} {}\n",
                    admitted + rejected,
                    active.get(label).copied().unwrap_or_default()
                ));
            }
        }
        text.push_str(&format!(
            "kasu_server_active {}\nkasu_server_pending {}\nkasu_server_metrics_active {}\nkasu_server_client_entries {}\n",
            self.limits.active - self.active.available_permits(),
            self.limits.pending - self.pending.available_permits(),
            2 - self.scrapes.available_permits(),
            self.buckets
                .lock()
                .map(|b| b
                    .keys()
                    .filter(|k| matches!(k, Identity::Client(_)))
                    .count())
                .unwrap_or_default()
        ));
        text
    }
}
const CODES: [&str; 17] = [
    "ok",
    "cancelled",
    "unknown",
    "invalid_argument",
    "deadline_exceeded",
    "not_found",
    "already_exists",
    "permission_denied",
    "resource_exhausted",
    "failed_precondition",
    "aborted",
    "out_of_range",
    "unimplemented",
    "internal",
    "unavailable",
    "data_loss",
    "unauthenticated",
];
const BOUNDS: [u64; 9] = [
    1_000_000,
    5_000_000,
    10_000_000,
    50_000_000,
    100_000_000,
    500_000_000,
    1_000_000_000,
    5_000_000_000,
    u64::MAX,
];
struct MethodStats {
    requests: AtomicU64,
    active: AtomicU64,
    results: [AtomicU64; 17],
    duration: AtomicU64,
    buckets: [AtomicU64; 9],
    bytes: AtomicU64,
}
impl Default for MethodStats {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            active: AtomicU64::new(0),
            results: std::array::from_fn(|_| AtomicU64::new(0)),
            duration: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            bytes: AtomicU64::new(0),
        }
    }
}
pub(crate) struct ServerStats {
    methods: Mutex<BTreeMap<(bool, &'static str), Arc<MethodStats>>>,
    pub cas: [[AtomicU64; 5]; 4],
}
impl Default for ServerStats {
    fn default() -> Self {
        Self {
            methods: Mutex::new(BTreeMap::new()),
            cas: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}
impl ServerStats {
    fn begin(&self, grpc: bool, method: &'static str) -> Arc<Attempt> {
        let stats = self
            .methods
            .lock()
            .unwrap()
            .entry((grpc, method))
            .or_default()
            .clone();
        stats.requests.fetch_add(1, Ordering::Relaxed);
        Arc::new(Attempt {
            stats,
            start: Instant::now(),
            span: tracing::Span::current(),
            code: AtomicUsize::new(1),
            admitted: AtomicUsize::new(0),
        })
    }
    fn render(&self) -> String {
        use std::fmt::Write;
        let mut text = String::from(concat!(
            "# TYPE kasu_server_requests_total counter\n",
            "# TYPE kasu_server_method_active gauge\n",
            "# TYPE kasu_server_response_bytes_total counter\n",
            "# TYPE kasu_server_results_total counter\n",
            "# TYPE kasu_server_duration_seconds histogram\n",
            "# TYPE kasu_server_ref_cas_total counter\n",
            "# TYPE kasu_server_active gauge\n",
            "# TYPE kasu_server_pending gauge\n",
            "# TYPE kasu_server_metrics_active gauge\n",
            "# TYPE kasu_server_client_entries gauge\n",
            "# TYPE kasu_server_git_cache_hits_total counter\n",
            "# TYPE kasu_server_git_cache_misses_total counter\n",
            "# TYPE kasu_server_git_cache_bytes gauge\n",
        ));
        let methods = self.methods.lock().unwrap();
        for ((grpc, method), s) in methods.iter() {
            let label = format!(
                "transport=\"{}\",method=\"{method}\"",
                if *grpc { "grpc" } else { "http" }
            );
            let _ = writeln!(
                text,
                "kasu_server_requests_total{{{label}}} {}",
                s.requests.load(Ordering::Relaxed)
            );
            let _ = writeln!(
                text,
                "kasu_server_method_active{{{label}}} {}",
                s.active.load(Ordering::Relaxed)
            );
            let _ = writeln!(
                text,
                "kasu_server_response_bytes_total{{{label}}} {}",
                s.bytes.load(Ordering::Relaxed)
            );
            let mut count = 0;
            for (code, n) in CODES.iter().zip(&s.results) {
                let value = n.load(Ordering::Relaxed);
                count += value;
                let _ = writeln!(
                    text,
                    "kasu_server_results_total{{{label},code=\"{code}\"}} {value}"
                );
            }
            let _ = writeln!(
                text,
                "kasu_server_duration_seconds_count{{{label}}} {count}"
            );
            let _ = writeln!(
                text,
                "kasu_server_duration_seconds_sum{{{label}}} {}",
                s.duration.load(Ordering::Relaxed) as f64 / 1e9
            );
            for (bound, n) in BOUNDS.iter().zip(&s.buckets) {
                let le = if *bound == u64::MAX {
                    "+Inf".into()
                } else {
                    format!("{}", *bound as f64 / 1e9)
                };
                let _ = writeln!(
                    text,
                    "kasu_server_duration_seconds_bucket{{{label},le=\"{le}\"}} {}",
                    n.load(Ordering::Relaxed)
                );
            }
        }
        for (op, values) in ["native", "git", "mixed", "land"].iter().zip(&self.cas) {
            for (result, n) in ["matched", "mismatch", "replayed", "cancelled", "error"]
                .iter()
                .zip(values)
            {
                let _ = writeln!(
                    text,
                    "kasu_server_ref_cas_total{{operation=\"{op}\",result=\"{result}\"}} {}",
                    n.load(Ordering::Relaxed)
                );
            }
        }
        text
    }
}
struct Attempt {
    stats: Arc<MethodStats>,
    start: Instant,
    span: tracing::Span,
    code: AtomicUsize,
    admitted: AtomicUsize,
}
impl Attempt {
    fn finish(&self, code: usize) {
        self.code.store(code.min(16), Ordering::Relaxed);
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        kasu_telemetry::finish(
            &self.span,
            tonic::Code::from_i32(self.code.load(Ordering::Relaxed) as i32),
        );
        let elapsed = self.start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.stats.results[self.code.load(Ordering::Relaxed)].fetch_add(1, Ordering::Relaxed);
        self.stats.duration.fetch_add(elapsed, Ordering::Relaxed);
        for (bound, n) in BOUNDS.iter().zip(&self.stats.buckets) {
            if elapsed <= *bound {
                n.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self.admitted.load(Ordering::Relaxed) != 0 {
            self.stats.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
#[derive(Clone)]
pub(crate) struct RequestLease(Arc<LeaseInner>);
impl RequestLease {
    /// Hold a delegate concurrency slot for the lifetime of the request.
    fn with_delegate(self, slot: Option<OwnedSemaphorePermit>) -> Self {
        match Arc::try_unwrap(self.0) {
            Ok(mut inner) => {
                inner._delegate = slot;
                Self(Arc::new(inner))
            }
            Err(shared) => Self(shared),
        }
    }
}
struct LeaseInner {
    _identity: Option<OwnedSemaphorePermit>,
    _delegate: Option<OwnedSemaphorePermit>,
    active: Mutex<Option<OwnedSemaphorePermit>>,
    repository: Mutex<Option<OwnedSemaphorePermit>>,
    attempt: Arc<Attempt>,
}
tokio::task_local! {static LEASE:RequestLease;}
pub(crate) fn blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let lease = LEASE.try_with(Clone::clone).ok();
    kasu_telemetry::blocking(move || {
        let _lease = lease;
        work()
    })
}
struct ObservedBody<B: HttpBody<Data = Bytes>> {
    inner: Pin<Box<B>>,
    lease: RequestLease,
    grpc: bool,
    code: usize,
    done: bool,
}
impl<B: HttpBody<Data = Bytes>> HttpBody for ObservedBody<B> {
    type Data = Bytes;
    type Error = B::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(bytes) = frame.data_ref() {
                    this.lease
                        .0
                        .attempt
                        .stats
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                }
                if this.grpc
                    && let Some(trailers) = frame.trailers_ref()
                {
                    this.code = grpc_code(trailers).unwrap_or(this.code);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                this.done = true;
                this.lease.0.attempt.finish(this.code);
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                this.done = true;
                this.code = 13;
                this.lease.0.attempt.finish(13);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
impl<B: HttpBody<Data = Bytes>> Drop for ObservedBody<B> {
    fn drop(&mut self) {
        self.lease
            .0
            .attempt
            .finish(if self.done || self.inner.is_end_stream() {
                self.code
            } else {
                1
            });
    }
}
fn grpc_code(headers: &axum::http::HeaderMap) -> Option<usize> {
    headers
        .get("grpc-status")?
        .to_str()
        .ok()?
        .parse::<usize>()
        .ok()
        .filter(|c| *c <= 16)
}
fn http_code(status: axum::http::StatusCode) -> usize {
    match status.as_u16() {
        200..=299 => 0,
        400 | 405 | 413 | 415 => 3,
        401 => 16,
        403 => 7,
        404 => 5,
        408 | 504 => 4,
        409 => 10,
        429 => 8,
        503 => 14,
        _ => 13,
    }
}
fn reject(status: Status, grpc: bool) -> Response<Body> {
    let limited = status.code() == tonic::Code::ResourceExhausted;
    let unauthenticated = status.code() == tonic::Code::Unauthenticated;
    let mut response = if grpc {
        status.into_http::<Body>()
    } else {
        crate::transport::HttpError(status).into_response()
    };
    if !grpc {
        response.headers_mut().insert(
            "cache-control",
            axum::http::HeaderValue::from_static("no-store"),
        );
        if unauthenticated {
            response.headers_mut().insert(
                "www-authenticate",
                axum::http::HeaderValue::from_static("Basic realm=\"kasu\""),
            );
        }
    }
    if limited {
        response
            .headers_mut()
            .insert("retry-after", axum::http::HeaderValue::from_static("1"));
    }
    response
}
fn operation(path: &str, query: Option<&str>, grpc: bool) -> &'static str {
    if grpc {
        return match path.rsplit('/').next().unwrap_or("") {
            "GetChangeset" => "GetChangeset",
            "GetRootTree" => "GetRootTree",
            "GetBlobMetadata" => "GetBlobMetadata",
            "ResolveRef" => "ResolveRef",
            "ListRefs" => "ListRefs",
            "Log" => "Log",
            "IsAncestor" => "IsAncestor",
            "MergeBase" => "MergeBase",
            "Diff" => "Diff",
            "Blame" => "Blame",
            "ReadFile" => "ReadFile",
            "ListDir" => "ListDir",
            "FindChangesetByGitOid" => "FindGitOid",
            "UploadBlobs" => "UploadBlobs",
            "CreateChangeset" => "CreateChangeset",
            "Merge" => "Merge",
            "Rebase" => "Rebase",
            "UpdateRef" => "UpdateRef",
            "Land" => "Land",
            "GetTrees" => "GetTrees",
            "GetBlobs" => "GetBlobs",
            "CreateRepository" => "CreateRepository",
            "GetRepository" => "GetRepository",
            "ListRepositories" => "ListRepositories",
            "UpdateRepository" => "UpdateRepository",
            "RenameRepository" => "RenameRepository",
            "DeleteRepository" => "DeleteRepository",
            "ForkRepository" => "ForkRepository",
            "GetRepositoryUsage" => "GetRepositoryUsage",
            _ => "Unknown",
        };
    }
    match path {
        "/v1/changeset" => "GetChangeset",
        "/v1/root-tree" => "GetRootTree",
        "/v1/blob-metadata" => "GetBlobMetadata",
        "/v1/resolve-ref" => "ResolveRef",
        "/v1/refs" => "ListRefs",
        "/v1/log" => "Log",
        "/v1/is-ancestor" => "IsAncestor",
        "/v1/merge-base" => "MergeBase",
        "/v1/diff" => "Diff",
        "/v1/blame" => "Blame",
        "/v1/read-file" => "ReadFile",
        "/v1/list-dir" => "ListDir",
        "/v1/find-git-oid" => "FindGitOid",
        "/v1/upload-blobs" => "UploadBlobs",
        "/v1/create-changeset" => "CreateChangeset",
        "/v1/merge" => "Merge",
        "/v1/rebase" => "Rebase",
        "/v1/update-ref" => "UpdateRef",
        "/v1/land" => "Land",
        "/v1/guarded-land" => "GuardedLand",
        "/v1/admin/create-repository" => "CreateRepository",
        "/v1/admin/get-repository" => "GetRepository",
        "/v1/admin/list-repositories" => "ListRepositories",
        "/v1/admin/update-repository" => "UpdateRepository",
        "/v1/admin/rename-repository" => "RenameRepository",
        "/v1/admin/delete-repository" => "DeleteRepository",
        "/v1/admin/fork-repository" => "ForkRepository",
        "/v1/admin/repository-usage" => "GetRepositoryUsage",
        "/metrics" => "Metrics",
        p if p.ends_with("/info/refs") => {
            if query == Some("service=git-receive-pack") {
                "GitReceiveAdvert"
            } else {
                "GitUploadAdvert"
            }
        }
        p if p.ends_with("/git-upload-pack") => "GitUpload",
        p if p.ends_with("/git-receive-pack") => "GitReceive",
        _ => "Unknown",
    }
}
#[derive(Clone)]
pub struct ServerLayer {
    state: Arc<Backend>,
    grpc: bool,
}
impl ServerLayer {
    pub fn http(state: Arc<Backend>) -> Self {
        Self { state, grpc: false }
    }
    pub fn grpc(state: Arc<Backend>) -> Self {
        Self { state, grpc: true }
    }
}
#[derive(Clone)]
pub struct ObservedService<S> {
    inner: S,
    state: Arc<Backend>,
    grpc: bool,
}
impl<S> Layer<S> for ServerLayer {
    type Service = ObservedService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        ObservedService {
            inner,
            state: self.state.clone(),
            grpc: self.grpc,
        }
    }
}
impl<S: tonic::server::NamedService> tonic::server::NamedService for ObservedService<S> {
    const NAME: &'static str = S::NAME;
}
impl<S, ReqB, ResB> Service<Request<ReqB>> for ObservedService<S>
where
    S: Service<Request<ReqB>, Response = Response<ResB>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    ReqB: Send + 'static,
    ResB: HttpBody<Data = Bytes> + Send + 'static,
    ResB::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, mut request: Request<ReqB>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let state = self.state.clone();
        let grpc = self.grpc;
        let method = operation(request.uri().path(), request.uri().query(), grpc);
        let span = kasu_telemetry::server_span(method, if grpc { "grpc" } else { "http" }, |key| {
            let mut values = request.headers().get_all(key).iter();
            let first = values.next()?;
            if values.next().is_some() {
                return None;
            }
            first.to_str().ok()
        });
        Box::pin(
            async move {
                let attempt = state.runtime.stats.begin(grpc, method);
                let peer = request
                    .extensions()
                    .get::<axum::extract::ConnectInfo<SocketAddr>>()
                    .map(|p| p.0.ip())
                    .or_else(|| {
                        request
                            .extensions()
                            .get::<tonic::transport::server::TcpConnectInfo>()
                            .and_then(|p| p.remote_addr)
                            .map(|p| p.ip())
                    })
                    .map(|p| match p {
                        IpAddr::V6(ip) => ip.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(p),
                        _ => p,
                    });
                // A verified delegate is admitted as the browser and the account
                // it acts as, not as its own shared address.
                let delegation = {
                    let headers = request.headers();
                    let header = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned)
                    };
                    state.verify_delegation(&header, peer)
                };
                let delegation = match delegation {
                    Ok(delegation) => delegation,
                    Err(status) => {
                        attempt.finish(status.code() as usize);
                        return Ok(reject(status, grpc));
                    }
                };
                let mut delegate_slot = None;
                let client_key = match &delegation {
                    Some(verified) => {
                        match axum::http::HeaderValue::from_str(&verified.authorization) {
                            Ok(value) => {
                                request.headers_mut().insert("authorization", value);
                            }
                            Err(_) => {
                                let status = Status::internal("invalid delegated credential");
                                attempt.finish(status.code() as usize);
                                return Ok(reject(status, grpc));
                            }
                        }
                        match state.runtime.delegate_slot(verified.delegation.delegate) {
                            Ok(slot) => delegate_slot = Some(slot),
                            Err(status) => {
                                attempt.finish(status.code() as usize);
                                return Ok(reject(status, grpc));
                            }
                        }
                        Identity::Client(verified.client)
                    }
                    None => Identity::Client(peer),
                };
                let authorization = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok());
                let result = (|| {
                    let client_slots = state.runtime.bucket(client_key, Instant::now())?;
                    let identity = state.transport_identity(
                        authorization,
                        grpc && matches!(method, "GetTrees" | "GetBlobs"),
                    )?;
                    let slots = match identity {
                        Some(identity) => state.runtime.bucket(identity, Instant::now())?,
                        None => client_slots,
                    };
                    Ok::<_, Status>(slots)
                })();
                let slots = match result {
                    Ok(slots) => slots,
                    Err(status) => {
                        attempt.finish(status.code() as usize);
                        return Ok(reject(status, grpc));
                    }
                };
                let acquired = if method == "Metrics" {
                    state.runtime.scrape(attempt.clone())
                } else {
                    state.runtime.acquire(slots, attempt.clone()).await
                };
                let lease = match acquired {
                    Ok(lease) => lease.with_delegate(delegate_slot),
                    Err(status) => {
                        attempt.finish(status.code() as usize);
                        return Ok(reject(status, grpc));
                    }
                };
                let response = LEASE.scope(lease.clone(), inner.call(request)).await?;
                let code = if grpc {
                    grpc_code(response.headers()).unwrap_or(0)
                } else {
                    http_code(response.status())
                };
                let (parts, body) = response.into_parts();
                let done = body.is_end_stream();
                if done {
                    attempt.finish(code);
                }
                let body = ObservedBody {
                    inner: Box::pin(body),
                    lease,
                    grpc,
                    code,
                    done,
                };
                Ok(Response::from_parts(parts, Body::new(body)))
            }
            .instrument(span)
            .with_current_subscriber(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn runtime(limits: ServerLimits) -> Arc<Runtime> {
        Runtime::new(limits, Arc::new(ServerStats::default()))
    }
    fn backend(limits: ServerLimits) -> (tempfile::TempDir, Arc<Backend>) {
        let temp = tempfile::tempdir().unwrap();
        let meta = Arc::new(kasu_store::SqliteStore::memory().unwrap());
        let blobs = Arc::new(kasu_store::FsBlobStore::open(temp.path()).unwrap());
        let backend = Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            blobs,
            meta,
            Some("secret".into()),
        )
        .with_limits(limits)
        .unwrap();
        (temp, Arc::new(backend))
    }
    fn request(path: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap()
    }
    #[test]
    fn buckets_refill_and_eviction_cannot_reset_quota_or_active_identity() {
        let r = runtime(ServerLimits {
            max_clients: 1,
            client_burst: 3,
            client_rate_per_second: 1,
            idle_ms: 1000,
            ..Default::default()
        });
        let a = Identity::Client(Some("127.0.0.1".parse().unwrap()));
        let b = Identity::Client(Some("127.0.0.2".parse().unwrap()));
        let now = Instant::now();
        let held = r.bucket(a, now).unwrap();
        drop(r.bucket(a, now).unwrap());
        drop(r.bucket(a, now).unwrap());
        assert_eq!(
            r.bucket(a, now).unwrap_err().code(),
            tonic::Code::ResourceExhausted
        );
        // Idle TTL alone must not grant a fresh burst before the bucket refills.
        assert!(r.bucket(b, now + Duration::from_millis(1500)).is_err());
        assert!(r.bucket(b, now + Duration::from_secs(10)).is_err());
        drop(held);
        assert!(r.bucket(b, now + Duration::from_secs(10)).is_ok());
        assert_eq!(r.buckets.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn a_busy_repository_queues_without_holding_server_slots() {
        let r = runtime(ServerLimits {
            active: 2,
            per_identity_active: 8,
            pending: 8,
            queue_timeout_ms: 2000,
            per_repository_active: Some(1),
            per_repository_pending: Some(1),
            ..Default::default()
        });
        let busy = RepoKey::generate().unwrap();
        let quiet = RepoKey::generate().unwrap();
        let name = kasu_core::RepoName::new("team/busy").unwrap();
        let limits = crate::registry::RepositoryLimits::default();
        let slots = r.bucket(Identity::Operator, Instant::now()).unwrap();
        let lease = |r: Arc<Runtime>, slots: Arc<Semaphore>| async move {
            r.acquire(slots, r.stats.begin(false, "Log")).await.unwrap()
        };
        // The first busy request runs; the second queues on the repository.
        let first = lease(r.clone(), slots.clone()).await;
        LEASE
            .scope(first.clone(), r.admit_repository(&busy, &name, &limits))
            .await
            .unwrap();
        let second = lease(r.clone(), slots.clone()).await;
        assert_eq!(r.active.available_permits(), 0);
        let queued = tokio::spawn({
            let r = r.clone();
            let (busy, name, limits) = (busy.clone(), name.clone(), limits.clone());
            let second = second.clone();
            async move {
                LEASE
                    .scope(second, r.admit_repository(&busy, &name, &limits))
                    .await
            }
        });
        // Queuing released the server-wide slot, so another repository runs.
        while r.active.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
        let third = lease(r.clone(), slots.clone()).await;
        LEASE
            .scope(third.clone(), r.admit_repository(&quiet, &name, &limits))
            .await
            .unwrap();
        // The repository queue is bounded independently of the server queue.
        let fourth =
            tokio::time::timeout(Duration::from_millis(50), lease(r.clone(), slots.clone())).await;
        assert!(fourth.is_err(), "server slots are all in use");
        drop(third);
        let fourth = lease(r.clone(), slots.clone()).await;
        assert_eq!(
            LEASE
                .scope(fourth.clone(), r.admit_repository(&busy, &name, &limits))
                .await
                .unwrap_err()
                .message(),
            "repository request queue is full"
        );
        drop(fourth);
        // Finishing the running request admits the queued one, which takes a
        // server slot again before it proceeds.
        drop(first);
        queued.await.unwrap().unwrap();
        assert_eq!(r.active.available_permits(), 1);
        drop(second);
        assert_eq!(r.active.available_permits(), 2);
        assert!(
            r.metrics()
                .contains("kasu_server_repository_rejected_total{repository=\"other\"} 1")
        );
    }
    #[tokio::test]
    async fn bounded_queue_timeout_and_cancellation_release_permits() {
        let r = runtime(ServerLimits {
            active: 1,
            per_identity_active: 1,
            pending: 1,
            queue_timeout_ms: 20,
            ..Default::default()
        });
        let slots = r.bucket(Identity::Operator, Instant::now()).unwrap();
        let first = r
            .acquire(slots.clone(), r.stats.begin(false, "Log"))
            .await
            .unwrap();
        let waiting = tokio::spawn({
            let r = r.clone();
            let slots = slots.clone();
            async move { r.acquire(slots, r.stats.begin(false, "Log")).await }
        });
        while r.pending.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            r.acquire(slots.clone(), r.stats.begin(false, "Log"))
                .await
                .err()
                .unwrap()
                .message(),
            "request queue is full"
        );
        waiting.abort();
        assert!(waiting.await.err().unwrap().is_cancelled());
        assert_eq!(r.pending.available_permits(), 1);
        let result = r.acquire(slots.clone(), r.stats.begin(false, "Log")).await;
        assert_eq!(
            result.err().unwrap().message(),
            "request queue deadline exceeded"
        );
        assert_eq!(slots.available_permits(), 0);
        drop(first);
        assert_eq!(r.active.available_permits(), 1);
        assert_eq!(slots.available_permits(), 1);
        assert!(r.acquire(slots, r.stats.begin(false, "Log")).await.is_ok());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_blocking_handler_keeps_capacity_and_metrics_stays_available() {
        let (_temp, backend) = backend(ServerLimits {
            active: 1,
            per_identity_active: 1,
            pending: 1,
            queue_timeout_ms: 1000,
            ..Default::default()
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        let router = Router::new()
            .route(
                "/slow",
                get({
                    let started = started.clone();
                    move || {
                        let started = started.clone();
                        let wait = wait.clone();
                        async move {
                            blocking(move || {
                                started.notify_one();
                                wait.lock()
                                    .unwrap()
                                    .recv_timeout(Duration::from_secs(5))
                                    .unwrap();
                                "finished"
                            })
                            .await
                            .unwrap()
                        }
                    }
                }),
            )
            .route("/fast", get(|| async { "fast" }))
            .layer(ServerLayer::http(backend.clone()));
        let task = tokio::spawn(router.clone().oneshot(request("/slow")));
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(backend.runtime.active.available_permits(), 0);
        let pending = tokio::spawn(router.clone().oneshot(request("/fast")));
        while backend.runtime.pending.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
        let rejected = router.clone().oneshot(request("/fast")).await.unwrap();
        assert_eq!(rejected.status(), 429);
        assert_eq!(rejected.headers()["retry-after"], "1");
        let scrape = crate::http_router(backend.clone())
            .oneshot(request("/metrics"))
            .await
            .unwrap();
        assert_eq!(scrape.status(), 200);
        let metrics = String::from_utf8(
            scrape
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(metrics.contains("kasu_server_active 1\n"));
        assert!(metrics.contains("kasu_server_pending 1\n"));
        assert!(!metrics.contains("secret"));
        release.send(()).unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), 200);
        response.into_body().collect().await.unwrap();
        assert_eq!(backend.runtime.active.available_permits(), 1);
        assert!(backend.metrics().contains("code=\"cancelled\"} 1"));
    }
    #[tokio::test]
    async fn streaming_body_holds_lease_until_completion_or_drop() {
        let (_temp, backend) = backend(ServerLimits {
            active: 1,
            per_identity_active: 1,
            ..Default::default()
        });
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(1);
        let rx = Arc::new(Mutex::new(Some(rx)));
        let router = Router::new()
            .route(
                "/stream",
                get(move || {
                    let rx = rx.clone();
                    async move {
                        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(
                            rx.lock().unwrap().take().unwrap(),
                        ))
                    }
                }),
            )
            .layer(ServerLayer::http(backend.clone()));
        let mut body = router
            .oneshot(request("/stream"))
            .await
            .unwrap()
            .into_body();
        assert_eq!(backend.runtime.active.available_permits(), 0);
        tx.send(Ok(Bytes::from_static(b"payload"))).await.unwrap();
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "payload"
        );
        assert_eq!(backend.runtime.active.available_permits(), 0);
        drop(body);
        assert_eq!(backend.runtime.active.available_permits(), 1);
        let text = backend.metrics();
        assert!(text.contains(
            "kasu_server_response_bytes_total{transport=\"http\",method=\"Unknown\"} 7\n"
        ));
        assert!(text.contains("code=\"cancelled\"} 1"));
    }
    #[tokio::test]
    async fn socket_extensions_are_used_and_mapped_addresses_share_an_identity() {
        let (_temp, backend) = backend(ServerLimits::default());
        let router = Router::new().route("/", get(|| async { "ok" }));
        for peer in ["127.0.0.1:12", "[::ffff:127.0.0.1]:13"] {
            let mut q = request("/");
            q.extensions_mut().insert(axum::extract::ConnectInfo(
                peer.parse::<SocketAddr>().unwrap(),
            ));
            router
                .clone()
                .layer(ServerLayer::http(backend.clone()))
                .oneshot(q)
                .await
                .unwrap()
                .into_body()
                .collect()
                .await
                .unwrap();
        }
        let mut q = request("/");
        q.extensions_mut()
            .insert(tonic::transport::server::TcpConnectInfo {
                local_addr: None,
                remote_addr: Some("127.0.0.2:14".parse().unwrap()),
            });
        router
            .layer(ServerLayer::grpc(backend.clone()))
            .oneshot(q)
            .await
            .unwrap()
            .into_body()
            .collect()
            .await
            .unwrap();
        let buckets = backend.runtime.buckets.lock().unwrap();
        assert!(buckets.contains_key(&Identity::Client(Some("127.0.0.1".parse().unwrap()))));
        assert!(buckets.contains_key(&Identity::Client(Some("127.0.0.2".parse().unwrap()))));
        assert!(!buckets.contains_key(&Identity::Client(None)));
        assert_eq!(buckets.len(), 3); // two peer addresses and the authenticated principal
    }
    #[tokio::test]
    async fn metrics_has_a_separate_bounded_pool() {
        let (_temp, backend) = backend(ServerLimits {
            active: 1,
            per_identity_active: 1,
            ..Default::default()
        });
        let router = crate::http_router(backend.clone());
        let first = router.clone().oneshot(request("/metrics")).await.unwrap();
        let second = router.clone().oneshot(request("/metrics")).await.unwrap();
        assert_eq!(first.status(), 200);
        assert_eq!(second.status(), 200);
        assert_eq!(
            router
                .clone()
                .oneshot(request("/metrics"))
                .await
                .unwrap()
                .status(),
            429
        );
        assert_eq!(backend.runtime.scrapes.available_permits(), 0);
        assert_eq!(backend.runtime.active.available_permits(), 1);
        drop(first);
        drop(second);
        assert_eq!(
            router.oneshot(request("/metrics")).await.unwrap().status(),
            200
        );
    }
    #[test]
    fn config_is_bounded_strict_and_validated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("limits.json");
        for raw in [
            b"{\"active\":0}".as_slice(),
            b"{\"unrecognized\":1}",
            b"{\"active\":1,\"per_identity_active\":2}",
        ] {
            std::fs::write(&path, raw).unwrap();
            assert!(ServerLimits::load(&path).is_err());
        }
        std::fs::write(&path, vec![b' '; 16385]).unwrap();
        assert!(ServerLimits::load(&path).is_err());
        std::fs::write(&path, b"{\"active\":1,\"per_identity_active\":1}").unwrap();
        assert_eq!(ServerLimits::load(&path).unwrap().active, 1);
    }
}
