use crate::{Backend, backend::status};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use kasu_core::*;
use kasu_derived::{Generation, diff, lookup};
use kasu_git::*;
use kasu_store::*;
use serde_json::{Value, json};
use sha2::Sha256;
use std::result::Result;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tonic::Status;

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookEndpoint {
    pub id: String,
    pub url: String,
    pub secret_file: Option<PathBuf>,
}
#[derive(Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    pub repository_url: String,
    pub default_branch: String,
    pub endpoints: Vec<WebhookEndpoint>,
    pub poll_interval_ms: u64,
    pub timeout_ms: u64,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
    pub max_payload_bytes: usize,
}
impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            repository_url: String::new(),
            default_branch: "main".into(),
            endpoints: vec![],
            poll_interval_ms: 100,
            timeout_ms: 5000,
            retry_base_ms: 100,
            retry_max_ms: 60_000,
            max_payload_bytes: 25 * 1024 * 1024,
        }
    }
}
impl WebhookConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut config: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        for endpoint in &mut config.endpoints {
            if let Some(secret) = &mut endpoint.secret_file
                && secret.is_relative()
            {
                *secret = path.parent().unwrap_or(Path::new(".")).join(&*secret);
            }
        }
        Ok(config)
    }
}
fn valid_url(text: &str, repository: bool) -> bool {
    reqwest::Url::parse(text).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none()
            && (!repository || u.query().is_none())
    })
}
#[derive(Default, Clone)]
struct Paths {
    added: BTreeSet<Vec<u8>>,
    modified: BTreeSet<Vec<u8>>,
    removed: BTreeSet<Vec<u8>>,
}
impl Paths {
    fn extend(&mut self, other: &Self) {
        for path in &other.added {
            self.modified.remove(path);
            self.removed.remove(path);
        }
        for path in &other.modified {
            self.added.remove(path);
            self.removed.remove(path);
        }
        for path in &other.removed {
            self.added.remove(path);
            self.modified.remove(path);
        }
        self.added.extend(other.added.iter().cloned());
        self.modified.extend(other.modified.iter().cloned());
        self.removed.extend(other.removed.iter().cloned());
    }
    fn strings(paths: &BTreeSet<Vec<u8>>) -> Vec<String> {
        paths
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect()
    }
    fn raw(&self) -> Value {
        let encode = |paths: &BTreeSet<Vec<u8>>| {
            paths
                .iter()
                .map(|p| base64::engine::general_purpose::STANDARD.encode(p))
                .collect::<Vec<_>>()
        };
        json!({"added":encode(&self.added),"modified":encode(&self.modified),"removed":encode(&self.removed)})
    }
}
struct Event {
    sequence: u64,
    name: Vec<u8>,
    old: Option<GitOid>,
    new: Option<GitOid>,
    actor: Vec<u8>,
    time: i64,
}
impl Backend {
    fn changed_paths(&self, old: TreeId, new: TreeId) -> Result<Paths, Status> {
        let ops = self.ops();
        let mut paths = Paths::default();
        for change in diff(&ops.ctx.repo, old, new).map_err(status)? {
            let name = change.path().as_bytes().to_vec();
            match change {
                FileChange::Delete { .. } => {
                    paths.removed.insert(name);
                }
                FileChange::Set { ref path, .. } => {
                    if matches!(
                        lookup(&ops.ctx.repo, old, path).map_err(status)?,
                        Some(TreeEntry::Leaf(_))
                    ) {
                        paths.modified.insert(name);
                    } else {
                        paths.added.insert(name);
                    }
                }
            }
        }
        Ok(paths)
    }
    fn commit_payload(
        &self,
        id: ChangesetId,
        config: &WebhookConfig,
        distinct: bool,
        extra: &Paths,
    ) -> Result<Value, Status> {
        let ops = self.ops();
        let cs = ops.ctx.repo.changeset(id).map_err(status)?;
        let tree = ops.root_tree(id).map_err(status)?;
        let base = cs
            .parents
            .first()
            .map(|id| ops.root_tree(*id))
            .transpose()
            .map_err(status)?
            .unwrap_or(ops.ctx.repo.put_tree(&Tree::default()).map_err(status)?);
        let mut paths = self.changed_paths(base, tree)?;
        paths.extend(extra);
        let oid = ops
            .engine
            .derive::<GitCommit>(&ops.ctx, id, &ProjectionId::full(self.repository.clone()))
            .map_err(status)?
            .0;
        let timestamp = jiff::Timestamp::from_second(cs.committer.timestamp)
            .map(|ts| ts.to_string())
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into());
        Ok(
            json!({"id":oid.to_string(),"distinct":distinct,"message":String::from_utf8_lossy(&cs.message),"timestamp":timestamp,
            "url":format!("{}/commit/{oid}",config.repository_url.trim_end_matches(".git")),
            "author":{"name":String::from_utf8_lossy(&cs.author.name),"email":String::from_utf8_lossy(&cs.author.email)},
            "committer":{"name":String::from_utf8_lossy(&cs.committer.name),"email":String::from_utf8_lossy(&cs.committer.email)},
            "added":Paths::strings(&paths.added),"modified":Paths::strings(&paths.modified),"removed":Paths::strings(&paths.removed),
            "kasu":{"changeset":id.to_string(),"message_base64":base64::engine::general_purpose::STANDARD.encode(&cs.message),"timestamp":cs.committer.timestamp,"path_bytes":paths.raw()}}),
        )
    }
    fn event_target(&self, oid: Option<GitOid>) -> Result<(Option<ChangesetId>, TreeId), Status> {
        let ops = self.ops();
        let empty = ops.ctx.repo.put_tree(&Tree::default()).map_err(status)?;
        let Some(oid) = oid else {
            return Ok((None, empty));
        };
        let index = GitIndex {
            repo: ops.ctx.repo,
            projection: ProjectionId::full(self.repository.clone()),
        };
        let (oid, kind) = peel(&index, oid).map_err(status)?;
        match kind {
            GitKind::Commit => {
                let id = index
                    .find_changeset(oid)
                    .map_err(status)?
                    .ok_or_else(|| Status::internal("missing webhook changeset mapping"))?;
                Ok((Some(id), self.ops().root_tree(id).map_err(status)?))
            }
            GitKind::Tree => match index.require(oid).map_err(status)?.native {
                NativeTarget::Tree(id) => Ok((None, id)),
                _ => Err(Status::internal("missing webhook tree mapping")),
            },
            _ => Ok((None, empty)),
        }
    }
    fn next_webhook_event(
        &self,
        stream: WebhookStream,
        cursor: u64,
    ) -> Result<Option<Event>, Status> {
        let ops = self.ops();
        let projection = ProjectionId::full(self.repository.clone());
        let commit_oid = |id: Option<ChangesetId>| {
            id.map(|id| {
                ops.engine
                    .derive::<GitCommit>(&ops.ctx, id, &projection)
                    .map(|v| v.0)
            })
            .transpose()
            .map_err(status)
        };
        Ok(match stream {
            WebhookStream::Native => self
                .refs
                .tail(&self.repository, cursor, 1)
                .map_err(status)?
                .into_iter()
                .next()
                .map(|log| {
                    Ok::<_, Status>(Event {
                        sequence: log.seq,
                        name: log.name,
                        old: commit_oid(log.old)?,
                        new: commit_oid(log.new)?,
                        actor: log.actor,
                        time: log.time,
                    })
                })
                .transpose()?,
            WebhookStream::Git => self
                .refs
                .tail_git(&self.repository, cursor, 1)
                .map_err(status)?
                .into_iter()
                .next()
                .map(|log| {
                    let direct = |value: Option<GitRef>| match value {
                        Some(GitRef::Direct { target, .. }) => Some(target),
                        _ => None,
                    };
                    Event {
                        sequence: log.seq,
                        name: log.name,
                        old: direct(log.old),
                        new: direct(log.new),
                        actor: log.actor,
                        time: log.time,
                    }
                }),
        })
    }
    fn webhook_body(&self, event: &Event, config: &WebhookConfig) -> Result<Vec<u8>, Status> {
        if !event.name.starts_with(b"refs/") {
            return Ok(Vec::new());
        }
        let ops = self.ops();
        let (old, old_tree) = self.event_target(event.old)?;
        let (new, new_tree) = self.event_target(event.new)?;
        let final_paths = self.changed_paths(old_tree, new_tree)?;
        let mut previous = BTreeSet::new();
        let mut pending: Vec<_> = old.into_iter().collect();
        while let Some(id) = pending.pop() {
            if previous.insert(id) {
                pending.extend(ops.ctx.repo.changeset(id).map_err(status)?.parents);
            }
        }
        let mut added = BTreeSet::new();
        let mut pending: Vec<_> = new.into_iter().collect();
        while let Some(id) = pending.pop() {
            if !previous.contains(&id) && added.insert(id) {
                pending.extend(ops.ctx.repo.changeset(id).map_err(status)?.parents);
            }
        }
        let mut ordered = added
            .iter()
            .map(|id| {
                ops.engine
                    .derive::<Generation>(&ops.ctx, *id, &())
                    .map(|g| (g.0, *id))
            })
            .collect::<kasu_core::Result<Vec<_>>>()
            .map_err(status)?;
        ordered.sort();
        let size = ordered.len();
        let mut commits = Vec::new();
        let empty_paths = Paths::default();
        for (_, id) in ordered.into_iter().skip(size.saturating_sub(2048)) {
            commits.push(self.commit_payload(
                id,
                config,
                true,
                if Some(id) == new {
                    &final_paths
                } else {
                    &empty_paths
                },
            )?);
        }
        // Force-rewinds and deletes have no new distinct commits, but consumers
        // still need the final changed paths to invalidate their path caches.
        if commits.is_empty()
            && let Some(id) = new.or(old)
        {
            commits.push(self.commit_payload(id, config, false, &final_paths)?);
        }
        let forced = match (old, new) {
            (Some(a), Some(b)) => {
                !ops.is_ancestor(a, b).map_err(status)?
                    || (event.name.starts_with(b"refs/tags/") && event.old != event.new)
            }
            _ => event.old.is_some() && event.new.is_some() && event.old != event.new,
        };
        let name = String::from_utf8_lossy(&self.repository);
        let html = config.repository_url.trim_end_matches(".git");
        let payload = json!({"ref":String::from_utf8_lossy(&event.name),"before":event.old.unwrap_or(GitOid([0;20])).to_string(),"after":event.new.unwrap_or(GitOid([0;20])).to_string(),
            "created":event.old.is_none(),"deleted":event.new.is_none(),"forced":forced,"size":size,"distinct_size":size,"commits":commits,
            "head_commit":new.map(|id|self.commit_payload(id,config,true,&Paths::default())).transpose()?,
            "repository":{"name":name,"full_name":format!("kasu/{name}"),"html_url":html,"url":html,"clone_url":config.repository_url,"git_url":config.repository_url,"default_branch":config.default_branch},
            "pusher":{"name":String::from_utf8_lossy(&event.actor)},"sender":{"login":"kasu"},
            "kasu":{"sequence":event.sequence,"ref_base64":base64::engine::general_purpose::STANDARD.encode(&event.name),"time":event.time,"commits_truncated":size>2048,"final_path_bytes":final_paths.raw()}});
        let body = serde_json::to_vec(&payload)
            .map_err(|_| Status::internal("webhook serialization failed"))?;
        if body.len() > config.max_payload_bytes {
            return Err(Status::resource_exhausted(
                "webhook body exceeds configured size limit",
            ));
        }
        Ok(body)
    }
}

pub fn webhook_signature(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts keys of all sizes");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}
pub fn webhook_delivery_id(
    repo: &[u8],
    consumer: &str,
    stream: WebhookStream,
    sequence: u64,
) -> String {
    let mut input = Encoder::default();
    input.raw(b"kasu.webhook/v1");
    input.bytes(repo);
    input.bytes(consumer.as_bytes());
    input.i64(stream.code());
    input.u64(sequence);
    let mut bytes = ObjectId::hash(&input.0).0;
    bytes[6] = (bytes[6] & 15) | 0x80;
    bytes[8] = (bytes[8] & 63) | 0x80;
    let hex = hex::encode(&bytes[..16]);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
struct Endpoint {
    id: String,
    url: reqwest::Url,
    secret: Option<Vec<u8>>,
}
pub struct WebhookDispatcher {
    backend: Arc<Backend>,
    store: Arc<dyn WebhookStore>,
    config: Arc<WebhookConfig>,
    endpoints: Vec<Endpoint>,
    client: reqwest::Client,
}
#[derive(Debug, Eq, PartialEq)]
pub enum WebhookOutcome {
    Idle,
    Deferred,
    Acknowledged(u64),
    Retrying(u64),
}
impl WebhookDispatcher {
    pub fn new(
        backend: Arc<Backend>,
        store: Arc<dyn WebhookStore>,
        config: WebhookConfig,
    ) -> Result<Self, Status> {
        if !valid_url(&config.repository_url, true)
            || config.endpoints.is_empty()
            || config.endpoints.len() > 32
            || !(10..=60_000).contains(&config.poll_interval_ms)
            || !(1..=30_000).contains(&config.timeout_ms)
            || config.retry_base_ms == 0
            || config.retry_max_ms < config.retry_base_ms
            || config.retry_max_ms > 3_600_000
            || !(1024..=128 * 1024 * 1024).contains(&config.max_payload_bytes)
        {
            return Err(Status::invalid_argument("invalid webhook configuration"));
        }
        let mut names = BTreeSet::new();
        let mut endpoints = Vec::new();
        for e in &config.endpoints {
            if e.id.is_empty()
                || e.id.len() > 128
                || !e
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                || !names.insert(&e.id)
                || !valid_url(&e.url, false)
            {
                return Err(Status::invalid_argument(
                    "invalid or duplicate webhook endpoint",
                ));
            }
            let secret = e
                .secret_file
                .as_ref()
                .map(|path| {
                    std::fs::read(path)
                        .map_err(|_| Status::invalid_argument("cannot read webhook secret file"))
                })
                .transpose()?;
            if secret
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 65_536)
            {
                return Err(Status::invalid_argument("invalid webhook secret size"));
            }
            endpoints.push(Endpoint {
                id: e.id.clone(),
                url: reqwest::Url::parse(&e.url).unwrap(),
                secret,
            });
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(config.timeout_ms))
            .user_agent("kasu-webhook/0.1.0")
            .build()
            .map_err(|_| Status::internal("cannot construct webhook HTTP client"))?;
        Ok(Self {
            backend,
            store,
            config: Arc::new(config),
            endpoints,
            client,
        })
    }
    pub async fn dispatch_once(
        &self,
        endpoint: usize,
        stream: WebhookStream,
        now_ms: i64,
    ) -> Result<WebhookOutcome, Status> {
        let started = std::time::Instant::now();
        let endpoint = self
            .endpoints
            .get(endpoint)
            .ok_or_else(|| Status::invalid_argument("unknown webhook endpoint"))?;
        let store = self.store.clone();
        let backend = self.backend.clone();
        let consumer = endpoint.id.clone();
        let config = self.config.clone();
        let state = crate::admission::blocking(move || {
            let current = store
                .webhook_state(&backend.repository, &consumer, stream)
                .map_err(status)?;
            if current.pending.is_some() {
                return Ok::<_, Status>(current);
            }
            let Some(event) = backend.next_webhook_event(stream, current.cursor)? else {
                return Ok(current);
            };
            let body = backend.webhook_body(&event, &config)?;
            store
                .prepare_webhook(
                    &backend.repository,
                    &consumer,
                    stream,
                    current.cursor,
                    event.sequence,
                    &body,
                )
                .map_err(status)
        })
        .await
        .map_err(|_| Status::internal("webhook preparation worker failed"))??;
        let Some(delivery) = state.pending else {
            return Ok(WebhookOutcome::Idle);
        };
        if delivery.retry_at_ms > now_ms {
            return Ok(WebhookOutcome::Deferred);
        }
        let sequence = delivery.sequence;
        let sent = !delivery.body.is_empty();
        let id = webhook_delivery_id(&self.backend.repository, &endpoint.id, stream, sequence);
        let success = if delivery.body.is_empty() {
            true
        } else {
            let mut request = self
                .client
                .post(endpoint.url.clone())
                .header("content-type", "application/json")
                .header("X-GitHub-Event", "push")
                .header("X-GitHub-Delivery", &id);
            if let Some(secret) = &endpoint.secret {
                request = request.header(
                    "X-Hub-Signature-256",
                    webhook_signature(secret, &delivery.body),
                );
            }
            let response = request.body(delivery.body).send().await;
            let success = response
                .as_ref()
                .is_ok_and(|response| response.status().is_success());
            if !success {
                tracing::warn!(endpoint=%endpoint.id,stream=?stream,sequence,status=?response.as_ref().ok().map(|response|response.status().as_u16()),"webhook delivery will retry");
            }
            success
        };
        let retry = if success {
            None
        } else {
            let cap = self
                .config
                .retry_base_ms
                .saturating_mul(1u64 << delivery.attempts.min(16))
                .min(self.config.retry_max_ms);
            let seed = ObjectId::hash(format!("{id}/{}", delivery.attempts).as_bytes());
            let jitter = u64::from_be_bytes(seed.0[..8].try_into().unwrap()) % (cap / 2 + 1);
            Some(
                now_ms
                    .saturating_add(started.elapsed().as_millis().min(i64::MAX as u128) as i64)
                    .saturating_add((cap / 2 + jitter).max(1) as i64),
            )
        };
        let store = self.store.clone();
        let repo = self.backend.repository.clone();
        let consumer = endpoint.id.clone();
        let updated = crate::admission::blocking(move || {
            store
                .finish_webhook(&repo, &consumer, stream, sequence, retry)
                .map_err(status)
        })
        .await
        .map_err(|_| Status::internal("webhook acknowledgement worker failed"))??;
        if success && sent && updated {
            tracing::info!(endpoint=%endpoint.id,stream=?stream,sequence,"webhook acknowledged");
        }
        Ok(if success {
            WebhookOutcome::Acknowledged(sequence)
        } else {
            WebhookOutcome::Retrying(sequence)
        })
    }
    pub async fn run(self: Arc<Self>, shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut workers = tokio::task::JoinSet::new();
        for endpoint in 0..self.endpoints.len() {
            for stream in [WebhookStream::Native, WebhookStream::Git] {
                let dispatcher = self.clone();
                let mut stop = shutdown.clone();
                workers.spawn(async move {
                loop {
                    if *stop.borrow() {break;}
                    let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64;
                    let outcome=tokio::select! {r=dispatcher.dispatch_once(endpoint,stream,now)=>r,_=stop.changed()=>break};
                    if matches!(outcome,Ok(WebhookOutcome::Acknowledged(_))) {continue;}
                    if let Err(error)=outcome {tracing::warn!(endpoint=%dispatcher.endpoints[endpoint].id,stream=?stream,code=?error.code(),"webhook worker will retry");}
                    tokio::select! {_=tokio::time::sleep(Duration::from_millis(dispatcher.config.poll_interval_ms))=>{},_=stop.changed()=>break}
                }
            });
            }
        }
        while let Some(result) = workers.join_next().await {
            if result.is_err() {
                tracing::error!("webhook worker stopped unexpectedly");
            }
        }
    }
}
