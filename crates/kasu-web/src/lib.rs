//! kasu-web: the browser interface of kasu. A stateless client of
//! kasu-server's public RPCs; the server makes every authorization decision.
pub mod auth;
pub mod config;
pub mod i18n;
pub mod render;
pub mod rev;
pub mod upstream;

mod cache;
mod routes;

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub use config::Config;

/// An embedded asset served at `/-/static/<hash>/<name>`.
pub struct Asset {
    pub name: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub hash: String,
}
impl Asset {
    fn new(name: &'static str, content_type: &'static str, body: Vec<u8>) -> Self {
        let hash = blake3::hash(&body).to_hex()[..16].to_owned();
        Self {
            name,
            content_type,
            body,
            hash,
        }
    }
    pub fn url(&self) -> String {
        format!("/-/static/{}/{}", self.hash, self.name)
    }
}
pub struct Assets {
    pub css: Asset,
    pub js: Asset,
    pub favicon: Asset,
}
impl Assets {
    fn load() -> Self {
        let mut css = include_bytes!("../assets/app.css").to_vec();
        css.extend(render::highlight::Highlighter::css().into_bytes());
        Self {
            css: Asset::new("app.css", "text/css; charset=utf-8", css),
            js: Asset::new(
                "app.js",
                "text/javascript; charset=utf-8",
                include_bytes!("../assets/app.js").to_vec(),
            ),
            favicon: Asset::new(
                "favicon.svg",
                "image/svg+xml",
                include_bytes!("../assets/favicon.svg").to_vec(),
            ),
        }
    }
}

/// Request count and total seconds per (view, status).
type RequestStats = HashMap<(&'static str, u16), (u64, f64)>;
/// Prometheus counters; repository and account names are never labels.
#[derive(Default)]
pub struct Metrics {
    pub requests: Mutex<RequestStats>,
    pub upstream_errors: AtomicU64,
    pub logins: [AtomicU64; 2],
    pub cache: [AtomicU64; 2],
}
impl Metrics {
    pub fn observe(&self, view: &'static str, status: u16, seconds: f64) {
        if let Ok(mut map) = self.requests.lock() {
            let entry = map.entry((view, status)).or_default();
            entry.0 += 1;
            entry.1 += seconds;
        }
    }
    fn render(&self) -> String {
        let mut out = String::from(
            "# TYPE kasu_web_requests_total counter\n# TYPE kasu_web_request_seconds_sum counter\n",
        );
        if let Ok(map) = self.requests.lock() {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            for ((view, status), (count, seconds)) in entries {
                out.push_str(&format!(
                    "kasu_web_requests_total{{view=\"{view}\",status=\"{status}\"}} {count}\nkasu_web_request_seconds_sum{{view=\"{view}\",status=\"{status}\"}} {seconds:.6}\n"
                ));
            }
        }
        out.push_str(&format!(
            "kasu_web_upstream_errors_total {}\nkasu_web_logins_total{{result=\"success\"}} {}\nkasu_web_logins_total{{result=\"failure\"}} {}\nkasu_web_cache_hits_total {}\nkasu_web_cache_misses_total {}\n",
            self.upstream_errors.load(Ordering::Relaxed),
            self.logins[0].load(Ordering::Relaxed),
            self.logins[1].load(Ordering::Relaxed),
            self.cache[0].load(Ordering::Relaxed),
            self.cache[1].load(Ordering::Relaxed),
        ));
        out
    }
}

pub struct App {
    pub config: Config,
    pub upstream: upstream::Upstream,
    pub highlighter: Arc<render::highlight::Highlighter>,
    pub assets: Assets,
    pub metrics: Metrics,
    pub cache: cache::Cache,
    /// MiB of raw-file responses being fetched or sent.
    pub(crate) raw_downloads: Arc<tokio::sync::Semaphore>,
    logins: Mutex<LoginAttempts>,
}
#[derive(Default)]
struct LoginAttempts {
    minute: i64,
    clients: HashMap<IpAddr, u32>,
}
impl App {
    pub fn new(config: Config) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let upstream = upstream::Upstream::new(&config.server, config.delegate_token.clone())?;
        Ok(Arc::new(Self {
            highlighter: Arc::new(render::highlight::Highlighter::new(
                config.file.limits.highlight_concurrency,
            )),
            cache: cache::Cache::new(config.file.cache.bytes),
            raw_downloads: Arc::new(tokio::sync::Semaphore::new(256)),
            assets: Assets::load(),
            metrics: Metrics::default(),
            logins: Mutex::new(LoginAttempts::default()),
            upstream,
            config,
        }))
    }
    /// Count a login attempt; false once the client exceeds its budget.
    pub fn login_allowed(&self, client: Option<IpAddr>) -> bool {
        let Some(client) = client else {
            return true;
        };
        let client = match config::canonical(client) {
            IpAddr::V6(ip) => IpAddr::V6((u128::from(ip) & (u128::MAX << 64)).into()),
            ip => ip,
        };
        let minute = auth::now() / 60;
        let Ok(mut logins) = self.logins.lock() else {
            return false;
        };
        if logins.minute != minute {
            logins.clients.clear();
            logins.minute = minute;
        }
        if logins.clients.len() >= 65_536 && !logins.clients.contains_key(&client) {
            return false;
        }
        let entry = logins.clients.entry(client).or_default();
        *entry = entry.saturating_add(1);
        *entry <= self.config.file.auth.login_attempts_per_minute
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/-/static/{hash}/{file}", get(static_asset))
        .route("/-/healthz", get(|| async { "ok" }))
        .route("/-/readyz", get(ready))
        .route("/-/metrics", get(metrics))
        .fallback(routes::dispatch)
        .layer(axum::extract::DefaultBodyLimit::max(
            app.config.file.limits.upload_total_bytes as usize + 1024 * 1024,
        ))
        .layer(middleware::from_fn_with_state(
            app.clone(),
            security_headers,
        ))
        .with_state(app)
}

async fn static_asset(
    State(app): State<Arc<App>>,
    Path((hash, file)): Path<(String, String)>,
) -> Response {
    let assets = &app.assets;
    match [&assets.css, &assets.js, &assets.favicon]
        .into_iter()
        .find(|a| a.name == file)
    {
        Some(asset) if asset.hash == hash => (
            [
                (header::CONTENT_TYPE, asset.content_type),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            asset.body.clone(),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn ready(State(app): State<Arc<App>>) -> Response {
    let request = app.upstream.request(
        &upstream::Caller::default(),
        kasu_proto::v1::WhoAmIRequest {},
        false,
    );
    match app.upstream.identity().who_am_i(request).await {
        Ok(_) => "ready".into_response(),
        // The server answered; authentication failures still prove reachability.
        Err(status)
            if matches!(
                status.code(),
                tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
            ) =>
        {
            "ready".into_response()
        }
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "upstream unavailable").into_response(),
    }
}
async fn metrics(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(token) = &app.config.metrics_token else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !auth::session::constant_eq(presented.as_bytes(), token.as_bytes()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        app.metrics.render(),
    )
        .into_response()
}
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; connect-src 'self'";
async fn security_headers(
    State(app): State<Arc<App>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let mut set = |name: &'static str, value: &'static str| {
        if !headers.contains_key(name) {
            headers.insert(name, HeaderValue::from_static(value));
        }
    };
    set("content-security-policy", CSP);
    set("x-content-type-options", "nosniff");
    set("referrer-policy", "same-origin");
    set("cross-origin-opener-policy", "same-origin");
    set("cross-origin-resource-policy", "same-origin");
    if app.config.secure() {
        set("strict-transport-security", "max-age=31536000");
    }
    response
}
