/// Call `client.method(message)` acting as the viewer, retrying a read once
/// when the server is unavailable. The message expression runs per attempt.
macro_rules! rpc {
    ($ctx:expr, $client:ident . $method:ident ($message:expr)) => {
        $crate::upstream::read(|| {
            let mut client = $ctx.upstream().$client();
            let request = $ctx.req($message);
            async move { client.$method(request).await }
        })
    };
}

mod account;
mod admin;
mod browse;
mod review;
mod write;

use crate::{
    App,
    auth::{Identity, Refusal, Viewer, csrf, now},
    i18n::{Lang, Tr},
    rev::{self, Route},
    upstream::{self, Caller},
};
use askama::Template;
use axum::{
    body::{Body, Bytes},
    extract::{ConnectInfo, FromRequest, Multipart, State},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use kasu_proto::v1 as p;
use prost::Message;
use std::{net::SocketAddr, sync::Arc, time::Instant};
use tonic::{Code, Status};

/// Why a page could not be shown.
#[derive(Debug)]
pub enum WebError {
    NotFound,
    LoginRequired,
    Forbidden(String),
    BadRequest(String),
    Conflict(String),
    Busy,
    /// The server is still deriving data for this view.
    Pending,
    Upstream,
}
impl WebError {
    /// Map an error of a read: missing and forbidden are indistinguishable.
    pub fn read(status: Status) -> Self {
        match status.code() {
            Code::NotFound | Code::PermissionDenied => Self::NotFound,
            _ => Self::write(status),
        }
    }
    /// Map an error of a write, showing the server's reason.
    pub fn write(status: Status) -> Self {
        match status.code() {
            Code::Unauthenticated => Self::LoginRequired,
            Code::NotFound => Self::NotFound,
            Code::PermissionDenied => Self::Forbidden(status.message().into()),
            Code::InvalidArgument | Code::OutOfRange => Self::BadRequest(status.message().into()),
            Code::FailedPrecondition | Code::Aborted | Code::AlreadyExists => {
                Self::Conflict(status.message().into())
            }
            Code::ResourceExhausted => Self::Busy,
            Code::DeadlineExceeded | Code::Cancelled => Self::Pending,
            _ => {
                tracing::warn!(code = ?status.code(), message = %status.message(), "upstream request failed");
                Self::Upstream
            }
        }
    }
}
pub type Result<T, E = WebError> = std::result::Result<T, E>;
pub trait ReadExt<T> {
    fn read(self) -> Result<T>;
    fn wrote(self) -> Result<T>;
}
impl<T> ReadExt<T> for std::result::Result<T, Status> {
    fn read(self) -> Result<T> {
        self.map_err(WebError::read)
    }
    fn wrote(self) -> Result<T> {
        self.map_err(WebError::write)
    }
}

#[derive(Clone)]
pub struct Link {
    pub label: String,
    pub url: String,
    pub on: bool,
}
impl Link {
    pub fn new(label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            url: url.into(),
            on: false,
        }
    }
}
/// The resolved revision shown in a repository header.
pub struct RevBar {
    pub label: String,
    pub short: String,
    pub permalink: String,
    pub switch: String,
}
pub struct RepoNav {
    pub name: String,
    pub top: String,
    pub crumbs: Vec<Link>,
    pub tabs: Vec<Link>,
    pub badges: Vec<String>,
    pub clone: Vec<Link>,
    pub rev: Option<RevBar>,
}
/// Layout data every page has.
pub struct Page {
    pub title: String,
    pub t: Tr,
    pub lang: &'static str,
    pub account: Option<String>,
    pub login: bool,
    pub css: String,
    pub js: String,
    pub favicon: String,
    pub repo: Option<RepoNav>,
    pub notices: Vec<String>,
}
#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    page: Page,
    heading: String,
    detail: String,
}

/// Display preferences, kept in an unsigned cookie.
#[derive(Clone, Default)]
pub struct Prefs {
    pub lang: Option<Lang>,
    pub tz: Option<String>,
    pub split: bool,
}
impl Prefs {
    fn parse(value: &str) -> Self {
        let mut prefs = Self::default();
        for pair in value.split('&') {
            match pair.split_once('=') {
                Some(("lang", v)) => prefs.lang = Lang::parse(v),
                Some(("tz", v)) if !v.is_empty() && v.len() <= 64 => {
                    prefs.tz = percent_encoding::percent_decode_str(v)
                        .decode_utf8()
                        .ok()
                        .map(|s| s.into_owned())
                        .filter(|s| jiff::tz::TimeZone::get(s).is_ok())
                }
                Some(("diff", "split")) => prefs.split = true,
                _ => {}
            }
        }
        prefs
    }
    pub fn encode(&self) -> String {
        let mut parts = Vec::new();
        if let Some(lang) = self.lang {
            parts.push(format!("lang={}", lang.code()));
        }
        if let Some(tz) = &self.tz {
            parts.push(format!("tz={}", rev::segment(tz.as_bytes())));
        }
        if self.split {
            parts.push("diff=split".into());
        }
        parts.join("&")
    }
}

/// Everything a handler needs about the current request.
pub struct Ctx {
    pub app: Arc<App>,
    pub viewer: Viewer,
    pub t: Tr,
    pub prefs: Prefs,
    pub method: Method,
    pub headers: HeaderMap,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: std::sync::Mutex<Option<Body>>,
    pub extensions: axum::http::Extensions,
    pub set_cookies: Vec<String>,
    pub now: i64,
}
impl Ctx {
    pub fn q(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn caller(&self) -> &Caller {
        &self.viewer.caller
    }
    pub fn req<T>(&self, message: T) -> tonic::Request<T> {
        self.app
            .upstream
            .request(&self.viewer.caller, message, false)
    }
    pub fn wreq<T>(&self, message: T) -> tonic::Request<T> {
        self.app
            .upstream
            .request(&self.viewer.caller, message, true)
    }
    pub fn upstream(&self) -> &upstream::Upstream {
        &self.app.upstream
    }
    pub fn csrf(&self, kind: &str) -> String {
        csrf::token(
            &self.app.config.session_key,
            &self.viewer.csrf_subject,
            kind,
            self.now,
        )
    }
    pub fn idempotency_key() -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the operating system provides randomness");
        hex::encode(bytes)
    }
    pub fn page(&self, title: impl Into<String>) -> Page {
        let assets = &self.app.assets;
        Page {
            title: title.into(),
            t: self.t,
            lang: self.t.lang(),
            account: self.viewer.account().map(str::to_owned),
            login: self.app.config.file.auth.pat_login && !self.viewer.signed_in(),
            css: assets.css.url(),
            js: assets.js.url(),
            favicon: assets.favicon.url(),
            repo: None,
            notices: Vec::new(),
        }
    }
    /// Render `template` with a cache policy: immutable views may be kept
    /// by the browser; never by shared caches.
    pub fn html(
        &mut self,
        template: &impl Template,
        status: StatusCode,
        immutable: bool,
    ) -> Response {
        let body = match template.render() {
            Ok(body) => body,
            Err(error) => {
                tracing::error!(%error, "template rendering failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        let mut response = (
            status,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (
                    header::CACHE_CONTROL,
                    if self.viewer.signed_in() {
                        "private, no-store"
                    } else if immutable {
                        "private, max-age=3600"
                    } else {
                        "private, no-cache"
                    },
                ),
            ],
            body,
        )
            .into_response();
        self.attach_cookies(&mut response);
        response
    }
    pub fn attach_cookies(&mut self, response: &mut Response) {
        for cookie in self.set_cookies.drain(..) {
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
        }
    }
    pub fn redirect(&mut self, to: &str) -> Response {
        let mut response = (
            StatusCode::SEE_OTHER,
            [
                (header::LOCATION, to.to_owned()),
                (header::CACHE_CONTROL, "no-store".into()),
            ],
        )
            .into_response();
        self.attach_cookies(&mut response);
        response
    }
    pub fn error(&mut self, error: WebError) -> Response {
        let t = self.t;
        let (status, heading, detail) = match error {
            WebError::NotFound => (StatusCode::NOT_FOUND, t.s("Not found"), String::new()),
            WebError::LoginRequired => {
                if self.viewer.signed_in() || !self.app.config.file.auth.pat_login {
                    // A stale session: drop it and ask to sign in again.
                    self.set_cookies.push(format!(
                        "{}=; Max-Age=0{}",
                        self.app.session_cookie(),
                        self.app.cookie_attributes()
                    ));
                }
                if self.method == Method::GET && self.app.config.file.auth.pat_login {
                    let next = rev::segment(self.path.as_bytes());
                    return self.redirect(&format!("/-/login?next={next}"));
                }
                (
                    StatusCode::UNAUTHORIZED,
                    t.s("You need to log in."),
                    String::new(),
                )
            }
            WebError::Forbidden(reason) => (StatusCode::FORBIDDEN, t.s("Forbidden"), reason),
            WebError::BadRequest(reason) => {
                (StatusCode::BAD_REQUEST, t.s("Invalid request"), reason)
            }
            WebError::Conflict(reason) => (
                StatusCode::CONFLICT,
                t.s("The request was rejected:"),
                reason,
            ),
            WebError::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                t.s("The server is busy; try again later."),
                String::new(),
            ),
            WebError::Pending => (
                StatusCode::SERVICE_UNAVAILABLE,
                t.s("This view is still being computed; reload in a few seconds."),
                String::new(),
            ),
            WebError::Upstream => {
                self.app
                    .metrics
                    .upstream_errors
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (
                    StatusCode::BAD_GATEWAY,
                    t.s("The upstream server failed."),
                    String::new(),
                )
            }
        };
        let page = ErrorPage {
            page: self.page(heading),
            heading: heading.to_owned(),
            detail,
        };
        let mut response = self.html(&page, status, false);
        if matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        ) {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        }
        response
    }
    /// The request body as form fields, after CSRF and origin checks.
    pub async fn form(&mut self, kind: &str) -> Result<Fields> {
        self.check_origin()?;
        let body = self
            .body
            .lock()
            .ok()
            .and_then(|mut b| b.take())
            .ok_or_else(|| WebError::BadRequest("missing body".into()))?;
        let limit = if matches!(kind, "login" | "preferences" | "logout") {
            16 * 1024
        } else {
            self.app.config.file.limits.upload_total_bytes as usize + 1024 * 1024
        };
        let bytes = axum::body::to_bytes(body, limit)
            .await
            .map_err(|_| WebError::BadRequest("the form is too large".into()))?;
        let pairs = form_urlencoded::parse(&bytes).into_owned().collect();
        let fields = Fields(pairs);
        self.check(kind, fields.get("csrf"))?;
        Ok(fields)
    }
    /// A multipart body; the caller checks CSRF on its `csrf` part.
    pub async fn multipart(&mut self) -> Result<Multipart> {
        self.check_origin()?;
        let body = self
            .body
            .lock()
            .ok()
            .and_then(|mut b| b.take())
            .ok_or_else(|| WebError::BadRequest("missing body".into()))?;
        let mut builder = Request::builder();
        if let Some(value) = self.headers.get(header::CONTENT_TYPE) {
            builder = builder.header(header::CONTENT_TYPE, value);
        }
        let mut request = builder
            .body(body)
            .map_err(|_| WebError::BadRequest("invalid body".into()))?;
        *request.extensions_mut() = self.extensions.clone();
        Multipart::from_request(request, &())
            .await
            .map_err(|_| WebError::BadRequest("invalid multipart form".into()))
    }
    /// Verify the CSRF token of `kind` and that the request comes from this
    /// site.
    pub fn check(&self, kind: &str, token: Option<&str>) -> Result<()> {
        self.check_origin()?;
        let valid = token.is_some_and(|token| {
            csrf::verify(
                &self.app.config.session_key,
                &self.viewer.csrf_subject,
                kind,
                token,
                self.now,
            )
        });
        if !valid {
            return Err(WebError::Forbidden(
                "the form expired; reload and try again".into(),
            ));
        }
        Ok(())
    }
    fn check_origin(&self) -> Result<()> {
        let origin = self
            .headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| {
                self.headers
                    .get(header::REFERER)
                    .and_then(|v| v.to_str().ok())
                    .map(|r| {
                        let rest = r
                            .split_once("://")
                            .map(|(s, rest)| (s, rest.split('/').next().unwrap_or_default()));
                        rest.map(|(s, host)| format!("{s}://{host}"))
                            .unwrap_or_default()
                    })
            });
        if origin.as_deref() != Some(self.app.config.public_url.as_str()) {
            return Err(WebError::Forbidden("cross-site request".into()));
        }
        Ok(())
    }
    /// Refuse writes for visitors who are not signed in.
    pub fn require_account(&self) -> Result<()> {
        if self.viewer.signed_in() || !self.app.upstream.delegated() {
            Ok(())
        } else {
            Err(WebError::LoginRequired)
        }
    }
}
/// Decoded form fields; repeated names keep every value.
pub struct Fields(pub Vec<(String, String)>);
impl Fields {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn text(&self, name: &str) -> String {
        self.get(name).unwrap_or_default().to_owned()
    }
    pub fn flag(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| !v.is_empty())
    }
    /// A hex-encoded byte field (paths and IDs carried through forms).
    pub fn hex(&self, name: &str) -> Result<Vec<u8>> {
        hex::decode(self.get(name).unwrap_or_default())
            .map_err(|_| WebError::BadRequest(format!("invalid {name}")))
    }
}

fn view_name(route: &Route) -> &'static str {
    match route {
        Route::Repo { .. } => "repository",
        Route::View { view, .. } => match view.as_str() {
            "tree" => "tree",
            "blob" => "blob",
            "raw" => "raw",
            "blame" => "blame",
            "log" => "log",
            "changeset" => "changeset",
            "compare" => "compare",
            "refs" => "refs",
            "git" => "git",
            "archive" => "archive",
            "edit" | "new" | "upload" | "delete" => "write",
            "reviews" => "review",
            "settings" => "settings",
            _ => "other",
        },
    }
}

pub async fn dispatch(State(app): State<Arc<App>>, request: Request<Body>) -> Response {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let path = parts.uri.path().to_owned();
    let query = parts.uri.query().map(url_pairs).unwrap_or_default();
    let prefs = crate::auth::cookie(&parts.headers, "kasu_prefs")
        .map(Prefs::parse)
        .unwrap_or_default();
    let lang = prefs.lang.unwrap_or_else(|| {
        Lang::negotiate(
            parts
                .headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
        )
    });
    let login_page = path == "/-/login" || path == "/-/logout";
    let viewer = if login_page {
        Ok(app.login_viewer(&parts.headers, peer))
    } else {
        app.viewer(&parts.headers, peer)
    };
    let viewer = match viewer {
        Ok(viewer) => viewer,
        Err(refusal) => {
            let status = match refusal {
                Refusal::LoginRequired
                    if parts.method == Method::GET && app.config.file.auth.pat_login =>
                {
                    return (
                        StatusCode::SEE_OTHER,
                        [(
                            header::LOCATION,
                            format!("/-/login?next={}", rev::segment(path.as_bytes())),
                        )],
                    )
                        .into_response();
                }
                Refusal::LoginRequired => StatusCode::UNAUTHORIZED,
                Refusal::Forbidden => StatusCode::FORBIDDEN,
            };
            return (status, "access denied").into_response();
        }
    };
    let mut ctx = Ctx {
        app: app.clone(),
        t: Tr(lang),
        prefs,
        method: parts.method.clone(),
        headers: parts.headers,
        path: path.clone(),
        query,
        body: std::sync::Mutex::new(Some(body)),
        extensions: parts.extensions,
        set_cookies: Vec::new(),
        now: now(),
        viewer,
    };
    if let Some(visitor) = &ctx.viewer.new_visitor {
        ctx.set_cookies.push(format!(
            "{}={visitor}{}",
            app.visitor_cookie(),
            app.cookie_attributes()
        ));
    }
    // Sliding expiry: refresh the session cookie at most every five minutes.
    if let Identity::Session(session) = &ctx.viewer.identity
        && ctx.now - session.last >= 300
    {
        let mut session = session.clone();
        session.last = ctx.now;
        ctx.set_cookies.push(format!(
            "{}={}{}",
            app.session_cookie(),
            session.encode(&app.config.session_key),
            app.cookie_attributes()
        ));
    }
    let (view, result) = route(&mut ctx, &path).await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => ctx.error(error),
    };
    ctx.attach_cookies(&mut response);
    app.metrics.observe(
        view,
        response.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    response
}
pub fn url_pairs(query: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}
async fn route(ctx: &mut Ctx, path: &str) -> (&'static str, Result<Response>) {
    let post = ctx.method == Method::POST;
    if ctx.method != Method::GET && !post {
        return (
            "other",
            Err(WebError::BadRequest("unsupported method".into())),
        );
    }
    match path {
        "/" => return ("repositories", browse::repositories(ctx).await),
        "/-/login" => return ("login", account::login(ctx).await),
        "/-/logout" => return ("logout", account::logout(ctx).await),
        "/-/preferences" => return ("preferences", account::preferences(ctx).await),
        "/-/new" => return ("new_repository", admin::new_repository(ctx).await),
        _ if path.starts_with("/-/") => return ("other", Err(WebError::NotFound)),
        _ => {}
    }
    let Some(route) = Route::parse(path) else {
        return ("other", Err(WebError::NotFound));
    };
    let view = view_name(&route);
    let result = match route {
        Route::Repo { repo } => browse::repository(ctx, repo).await,
        Route::View { repo, view, rest } => match (view.as_str(), post) {
            ("tree", false) => browse::tree(ctx, repo, rest).await,
            ("blob", false) => browse::blob(ctx, repo, rest).await,
            ("raw", false) => browse::raw(ctx, repo, rest).await,
            ("blame", false) => browse::blame(ctx, repo, rest).await,
            ("log", false) => browse::log(ctx, repo, rest).await,
            ("changeset", false) => browse::changeset(ctx, repo, rest).await,
            ("compare", false) => browse::compare(ctx, repo, rest).await,
            ("git", false) => browse::git(ctx, repo, rest).await,
            ("archive", false) => browse::archive(ctx, repo, rest).await,
            ("refs", _) => write::refs(ctx, repo, rest, post).await,
            ("edit" | "new" | "upload" | "delete", _) => {
                write::write(ctx, repo, view.clone(), rest, post).await
            }
            ("reviews", _) => review::reviews(ctx, repo, rest, post).await,
            ("settings", _) => admin::settings(ctx, repo, rest, post).await,
            _ => Err(WebError::NotFound),
        },
    };
    (view, result)
}

/// The repository a page is about, with the caller's advisory permissions.
pub struct Repo {
    pub name: String,
    pub record: p::Repository,
    pub cache_scope: Vec<u8>,
}
impl Repo {
    pub fn bytes(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }
    pub fn url(&self, view: &str, rev: Option<&[u8]>, path: Option<&[u8]>) -> String {
        rev::url(&self.name, view, rev, path)
    }
    pub fn top(&self) -> String {
        rev::repo_url(&self.name)
    }
    pub fn default_ref(&self) -> Vec<u8> {
        [b"refs/heads/".as_slice(), &self.record.default_branch].concat()
    }
}
/// A revision resolved to a Changeset ID.
pub struct Resolved {
    /// What the URL said (ref name or ID).
    pub requested: Vec<u8>,
    pub id: Vec<u8>,
    /// The full ref name when `requested` named a ref.
    pub reference: Option<Vec<u8>>,
}
impl Resolved {
    pub fn immutable(&self) -> bool {
        self.reference.is_none()
    }
    /// The segment to use in further links (keeps the user's choice).
    pub fn segment(&self) -> &[u8] {
        &self.requested
    }
    pub fn label(&self) -> String {
        match &self.reference {
            Some(name) => crate::render::text::lossy(
                name.strip_prefix(b"refs/heads/")
                    .or_else(|| name.strip_prefix(b"refs/tags/"))
                    .unwrap_or(name),
            ),
            None => crate::render::text::short(&self.id),
        }
    }
}
impl Ctx {
    pub async fn repo(&self, name: &str) -> Result<Repo> {
        if kasu_core::RepoName::new(name).is_err() {
            return Err(WebError::NotFound);
        }
        let record = rpc!(
            self,
            admin.get_repository(p::GetRepositoryRequest {
                name: name.to_owned(),
            })
        )
        .await
        .read()?;
        // Refresh the effective scope for every request, before using cached data.
        let permissions = rpc!(
            self,
            identity.get_permissions(p::PermissionsRequest {
                repo: name.as_bytes().to_vec(),
                refs: Vec::new(),
            })
        )
        .await
        .read()?;
        Ok(Repo {
            name: name.to_owned(),
            record,
            cache_scope: permissions.encode_to_vec(),
        })
    }
    pub async fn permissions(&self, repo: &Repo, refs: &[Vec<u8>]) -> Option<p::Permissions> {
        if self.app.upstream.delegated() && self.viewer.caller.act_as.is_none() {
            return None;
        }
        rpc!(
            self,
            identity.get_permissions(p::PermissionsRequest {
                repo: repo.bytes(),
                refs: refs.to_vec(),
            })
        )
        .await
        .ok()
    }
    /// Resolve a URL revision: a Changeset ID, a full ref, a branch, a tag.
    pub async fn resolve(&self, repo: &Repo, requested: &[u8]) -> Result<Resolved> {
        if requested.len() == 64
            && let Ok(id) = hex::decode(requested)
        {
            return Ok(Resolved {
                requested: requested.to_vec(),
                id,
                reference: None,
            });
        }
        let candidates: Vec<Vec<u8>> = if requested.starts_with(b"refs/") {
            vec![requested.to_vec()]
        } else {
            vec![
                [b"refs/heads/".as_slice(), requested].concat(),
                [b"refs/tags/".as_slice(), requested].concat(),
            ]
        };
        for name in candidates {
            let resolved = rpc!(
                self,
                metadata.resolve_ref(p::RefRequest {
                    repo: repo.bytes(),
                    name: name.clone(),
                })
            )
            .await
            .read()?;
            if let Some(id) = resolved.target {
                return Ok(Resolved {
                    requested: requested.to_vec(),
                    id,
                    reference: Some(name),
                });
            }
        }
        // A short Changeset ID, when no ref has this name.
        if (4..64).contains(&requested.len()) && requested.iter().all(u8::is_ascii_hexdigit) {
            let prefix = String::from_utf8_lossy(requested).into_owned();
            let found = rpc!(
                self,
                metadata.find_changesets_by_prefix(p::PrefixRequest {
                    repo: repo.bytes(),
                    prefix: prefix.clone(),
                })
            )
            .await
            .read()?;
            match found.ids.as_slice() {
                [id] => {
                    return Ok(Resolved {
                        requested: requested.to_vec(),
                        id: id.clone(),
                        reference: None,
                    });
                }
                [] => {}
                _ => {
                    return Err(WebError::BadRequest(format!(
                        "the changeset ID prefix {prefix} is ambiguous"
                    )));
                }
            }
        }
        Err(WebError::NotFound)
    }
    /// Header data of a repository page.
    pub fn nav(&self, repo: &Repo, tab: &str, resolved: Option<&Resolved>, admin: bool) -> RepoNav {
        let t = self.t;
        let rev = resolved
            .map(|r| r.segment().to_vec())
            .unwrap_or_else(|| repo.record.default_branch.clone());
        let mut tabs = vec![
            Link::new(t.s("Code"), repo.url("tree", Some(&rev), None)),
            Link::new(t.s("History"), repo.url("log", Some(&rev), None)),
            Link::new(t.s("Refs"), repo.url("refs", None, None)),
            Link::new(t.s("Reviews"), repo.url("reviews", None, None)),
        ];
        if admin {
            tabs.push(Link::new(t.s("Settings"), repo.url("settings", None, None)));
        }
        for (link, name) in tabs
            .iter_mut()
            .zip(["code", "log", "refs", "reviews", "settings"])
        {
            link.on = name == tab;
        }
        let mut badges = Vec::new();
        match p::RepositoryState::try_from(repo.record.state) {
            Ok(p::RepositoryState::ReadOnly) => badges.push(t.s("Read-only").to_owned()),
            Ok(p::RepositoryState::Archived) => badges.push(t.s("Archived").to_owned()),
            _ => {}
        }
        let urls = &self.app.config.file.clone_urls;
        let mut clone = Vec::new();
        if let Some(base) = &urls.http {
            clone.push(Link::new(
                "HTTP",
                format!("{}/{}.git", base.trim_end_matches('/'), repo.name),
            ));
        }
        if let Some(base) = &urls.ssh {
            clone.push(Link::new("SSH", format!("{base}{}.git", repo.name)));
        }
        RepoNav {
            name: repo.name.clone(),
            top: repo.top(),
            crumbs: Vec::new(),
            tabs,
            badges,
            clone,
            rev: resolved.map(|r| RevBar {
                label: r.label(),
                short: crate::render::text::short(&r.id),
                permalink: match Route::parse(&self.path) {
                    Some(Route::View { view, rest, .. }) => repo.url(
                        &view,
                        Some(hex::encode(&r.id).as_bytes()),
                        Some(&rest.into_iter().skip(1).collect::<Vec<_>>().join(&b'/')),
                    ),
                    _ => repo.url("tree", Some(hex::encode(&r.id).as_bytes()), None),
                },
                switch: format!("{}?select=1", repo.url("refs", None, None)),
            }),
        }
    }
    /// Breadcrumbs for `path` at `rev` in tree view.
    pub fn crumbs(&self, repo: &Repo, rev: &[u8], path: &[u8]) -> Vec<Link> {
        let mut crumbs = Vec::new();
        if path.is_empty() {
            return crumbs;
        }
        let segments: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
        for i in 0..segments.len() {
            let prefix = segments[..=i].join(&b'/');
            let url = if i + 1 == segments.len() {
                String::new()
            } else {
                repo.url("tree", Some(rev), Some(&prefix))
            };
            crumbs.push(Link::new(crate::render::text::lossy(segments[i]), url));
        }
        crumbs
    }
}
/// Read a multipart field fully, bounded by `limit`.
pub async fn field_bytes(field: axum::extract::multipart::Field<'_>, limit: u64) -> Result<Bytes> {
    let mut field = field;
    let mut out = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| WebError::BadRequest("invalid upload".into()))?
    {
        if (out.len() + chunk.len()) as u64 > limit {
            return Err(WebError::BadRequest("upload exceeds the size limit".into()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}
