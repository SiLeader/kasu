#![allow(dead_code)]
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::sync::Arc;
use tower::ServiceExt;

pub const ORIGIN: &str = "http://kasu.test";

pub struct Harness {
    pub _dir: tempfile::TempDir,
    pub store: Arc<SqliteStore>,
    pub backend: Arc<Backend>,
    pub app: Arc<kasu_web::App>,
    pub web: Router,
}
pub fn grant(
    patterns: &[&str],
    write: bool,
    admin: bool,
    dirs: Option<&[&[u8]]>,
) -> RepositoryGrant {
    RepositoryGrant {
        repositories: patterns
            .iter()
            .map(|p| RepoPattern::parse(p).unwrap())
            .collect(),
        write_policy: write.then(|| WritePolicy {
            allow_delete: true,
            ..Default::default()
        }),
        read_directories: dirs.map(|d| d.iter().map(|p| RepoPath::new(*p).unwrap()).collect()),
        admin,
    }
}
pub fn account(name: &str, grants: Vec<RepositoryGrant>) -> RepositoryAccount {
    RepositoryAccount {
        name: name.into(),
        token: format!("{name}-pat"),
        grants,
        ..Default::default()
    }
}
/// The standard cast: alice (writer, admin of team/*), bob (writer),
/// carol (reader), dave (reads docs/ only), public (anonymous, writable on
/// purpose to prove anonymity is read-only).
pub fn accounts() -> Vec<RepositoryAccount> {
    let mut alice = account("alice", vec![grant(&["team/**"], true, true, None)]);
    alice.create_repositories = vec![RepoPattern::parse("team/*").unwrap()];
    alice.display_name = Some("Alice A".into());
    alice.email = Some("alice@example.com".into());
    vec![
        alice,
        account("bob", vec![grant(&["team/**"], true, false, None)]),
        account("carol", vec![grant(&["team/app"], false, false, None)]),
        account(
            "dave",
            vec![grant(&["team/app"], false, false, Some(&[b"docs"]))],
        ),
        account("public", vec![grant(&["team/app"], true, false, None)]),
    ]
}
pub fn delegate() -> Delegate {
    Delegate {
        name: "kasu-web".into(),
        token: "web-delegate".into(),
        act_as: vec!["*".into()],
        allow_admin: true,
        allow_unbound: true,
        anonymous_account: Some("public".into()),
        trusted_peers: vec![],
    }
}
impl Harness {
    /// A delegated deployment with accounts and anonymous browsing.
    pub async fn delegated(repositories: &[&str]) -> Self {
        Self::start(
            repositories,
            |b| {
                b.with_binding_key(vec![9; 32])
                    .unwrap()
                    .with_write_policy(WritePolicy {
                        allow_delete: true,
                        ..Default::default()
                    })
                    .with_repository_accounts(accounts())
                    .unwrap()
                    .with_delegates(vec![delegate()])
                    .unwrap()
            },
            |c| {
                c.delegate_token = Some("web-delegate".into());
                c.file.auth.anonymous.enabled = true;
                c.file.auth.anonymous.account = Some("public".into());
            },
        )
        .await
    }
    pub async fn start(
        repositories: &[&str],
        backend: impl FnOnce(Backend) -> Backend,
        config: impl FnOnce(&mut kasu_web::Config),
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteStore::memory().unwrap());
        let blobs = Arc::new(FsBlobStore::open(dir.path()).unwrap());
        for repository in repositories {
            let record = RepositoryRecord::new(
                RepoKey::generate().unwrap(),
                RepoName::new(*repository).unwrap(),
                b"test",
                1,
            );
            store.create_repository(&record).unwrap();
        }
        let base = Backend::with_registry(
            store.clone(),
            blobs,
            store.clone(),
            store.clone(),
            Some("operator".into()),
        )
        .with_registry_ttl(std::time::Duration::from_millis(1))
        .with_reviews(store.clone());
        let backend = Arc::new(backend(base));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tonic::transport::Server::builder()
            .add_service(MetadataService::new(backend.clone()).into_server())
            .add_service(WriteService::new(backend.clone()).into_server())
            .add_service(AdminService::new(backend.clone()).into_server())
            .add_service(IdentityService::new(backend.clone()).into_server())
            .add_service(ReviewService::new(backend.clone()).into_server())
            .add_service(ArchiveService::new(backend.clone()).into_server());
        tokio::spawn(
            server.serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );
        let mut web_config = kasu_web::Config::local(&format!("http://{address}"));
        web_config.file.clone_urls.http = Some("https://kasu.example".into());
        config(&mut web_config);
        web_config.validate().unwrap();
        let app = kasu_web::App::new(web_config).unwrap();
        Self {
            _dir: dir,
            store,
            web: kasu_web::router(app.clone()),
            app,
            backend,
        }
    }
    pub async fn send(&self, request: Request<Body>) -> Response {
        let response = self.web.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 1 << 28)
            .await
            .unwrap()
            .to_vec();
        Response {
            status,
            headers,
            body,
        }
    }
    pub async fn get(&self, path: &str, session: Option<&Session>) -> Response {
        let mut request = Request::get(path);
        if let Some(session) = session {
            request = request.header("cookie", session.cookie());
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }
    pub async fn post(
        &self,
        path: &str,
        session: Option<&Session>,
        fields: &[(&str, &str)],
    ) -> Response {
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let mut request = Request::post(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("origin", ORIGIN);
        if let Some(session) = session {
            request = request.header("cookie", session.cookie());
        }
        self.send(request.body(Body::from(body)).unwrap()).await
    }
    /// Sign in with a PAT through the login form.
    pub async fn login(&self, account: &str) -> Session {
        let visitor = Session {
            cookies: vec![("kasu_visitor".into(), "0".repeat(32))],
        };
        let page = self.get("/-/login", Some(&visitor)).await;
        let csrf = page.field("csrf");
        let token = format!("{account}-pat");
        let response = self
            .post(
                "/-/login",
                Some(&visitor),
                &[("csrf", &csrf), ("token", &token), ("next", "/")],
            )
            .await;
        assert_eq!(
            response.status,
            StatusCode::SEE_OTHER,
            "{}",
            response.text()
        );
        let mut session = visitor;
        session.absorb(&response.headers);
        session
    }
    /// A fixed binding key shared by harnesses that model one server.
    pub fn backend_binding_key(&self) -> Vec<u8> {
        vec![9; 32]
    }
    /// Commit `files` on `refs/heads/main` of `repository` as the operator.
    pub async fn commit(
        &self,
        repository: &str,
        files: &[(&[u8], Option<&[u8]>)],
        message: &str,
    ) -> Vec<u8> {
        let mut files = files.to_vec();
        files.sort();
        let router = http_router(self.backend.clone());
        let call = |path: &'static str, body: serde_json::Value| {
            let router = router.clone();
            async move {
                let response = router
                    .oneshot(
                        Request::post(path)
                            .header("content-type", "application/json")
                            .header("authorization", "Bearer operator")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status();
                let body: serde_json::Value =
                    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 26).await.unwrap())
                        .unwrap_or_default();
                assert_eq!(status, StatusCode::OK, "{path} {body}");
                body
            }
        };
        let repo = repository.as_bytes().to_vec();
        let chunks: Vec<p::BlobUpload> = files
            .iter()
            .filter_map(|(_, c)| *c)
            .map(|content| p::BlobUpload {
                repo: repo.clone(),
                id: ObjectId::hash(content).0.to_vec(),
                offset: 0,
                data: content.to_vec(),
                last: true,
            })
            .collect();
        if !chunks.is_empty() {
            call(
                "/v1/upload-blobs",
                serde_json::to_value(p::UploadBlobsHttpRequest { chunks }).unwrap(),
            )
            .await;
        }
        let current = call(
            "/v1/resolve-ref",
            serde_json::to_value(p::RefRequest {
                repo: repo.clone(),
                name: b"refs/heads/main".to_vec(),
            })
            .unwrap(),
        )
        .await;
        let parent = current["target"]
            .as_array()
            .map(|_| bytes(&current["target"]));
        let signature = p::Signature {
            name: b"Tester".to_vec(),
            email: b"t@example.com".to_vec(),
            timestamp: 1_700_000_000,
            tz: b"+0900".to_vec(),
        };
        let cs = call(
            "/v1/create-changeset",
            serde_json::to_value(p::CreateChangesetRequest {
                repo: repo.clone(),
                parents: parent.iter().cloned().collect(),
                changes: files
                    .iter()
                    .map(|(path, content)| p::FileChange {
                        path: path.to_vec(),
                        entry: content.map(|c| p::Entry {
                            kind: Some(p::entry::Kind::File(p::File {
                                blob: ObjectId::hash(c).0.to_vec(),
                                executable: false,
                            })),
                        }),
                    })
                    .collect(),
                metadata: Some(p::CommitMetadata {
                    author: Some(signature.clone()),
                    committer: Some(signature),
                    message: message.as_bytes().to_vec(),
                }),
            })
            .unwrap(),
        )
        .await;
        let id = bytes(&cs["id"]);
        call(
            "/v1/update-ref",
            serde_json::to_value(p::UpdateRefRequest {
                repo,
                name: b"refs/heads/main".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/main".to_vec(),
                    target: parent,
                    version: current["version"].as_u64().unwrap_or(0),
                }),
                target: Some(id.clone()),
                force: false,
            })
            .unwrap(),
        )
        .await;
        id
    }
    pub fn set_config(&self, repository: &str, config: &str) {
        let entry = self
            .store
            .resolve_repository_name(&RepoName::new(repository).unwrap(), 0)
            .unwrap()
            .unwrap();
        let mut record = self.store.repository(&entry.key).unwrap().unwrap();
        record.config = config.as_bytes().to_vec();
        record.config_version += 1;
        self.store
            .update_repository(&record, "test", b"test", 2)
            .unwrap();
    }
}
pub fn bytes(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect()
}
#[derive(Clone, Debug, Default)]
pub struct Session {
    pub cookies: Vec<(String, String)>,
}
impl Session {
    pub fn cookie(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
    pub fn absorb(&mut self, headers: &HeaderMap) {
        for value in headers.get_all("set-cookie") {
            let text = value.to_str().unwrap();
            let pair = text.split(';').next().unwrap();
            let (name, value) = pair.split_once('=').unwrap();
            self.cookies.retain(|(k, _)| k != name);
            if !value.is_empty() {
                self.cookies.push((name.into(), value.into()));
            }
        }
    }
}
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}
impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    /// The values of `attribute` in tags whose text matches `pattern`
    /// (`name="x"`), in document order. Templates quote every attribute.
    fn attributes(&self, pattern: &str, attribute: &str) -> Vec<String> {
        let text = self.text();
        let mut out = Vec::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find('<') {
            let end = rest[start..]
                .find('>')
                .map(|e| start + e)
                .unwrap_or(rest.len());
            let tag = &rest[start..end];
            if tag.contains(pattern) {
                let key = format!(" {attribute}=\"");
                if let Some(at) = tag.find(&key) {
                    let value = &tag[at + key.len()..];
                    let value = &value[..value.find('"').unwrap_or(value.len())];
                    out.push(unescape(value));
                } else if attribute == "value" && tag.starts_with("<textarea") {
                    let body = &rest[end + 1..];
                    out.push(unescape(&body[..body.find("</textarea>").unwrap_or(0)]));
                }
            }
            rest = &rest[end.min(rest.len())..];
            if rest.len() <= 1 {
                break;
            }
            rest = &rest[1..];
        }
        out
    }
    /// The value of the first form field named `name`.
    pub fn field(&self, name: &str) -> String {
        self.fields(name)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no field {name} in {}", self.text()))
    }
    pub fn fields(&self, name: &str) -> Vec<String> {
        self.attributes(&format!(" name=\"{name}\""), "value")
    }
    /// How many tags carry `attribute="value"`.
    pub fn count(&self, attribute: &str, value: &str) -> usize {
        self.attributes(&format!(" {attribute}=\"{value}\""), attribute)
            .len()
    }
    pub fn links(&self) -> Vec<String> {
        self.attributes("<a ", "href")
    }
    pub fn location(&self) -> String {
        self.headers
            .get("location")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    }
}
fn unescape(text: &str) -> String {
    [
        ("&quot;", "\""),
        ("&#34;", "\""),
        ("&#39;", "'"),
        ("&lt;", "<"),
        ("&#60;", "<"),
        ("&gt;", ">"),
        ("&#62;", ">"),
        ("&#38;", "&"),
        ("&amp;", "&"),
    ]
    .iter()
    .fold(text.to_owned(), |text, (entity, plain)| {
        text.replace(entity, plain)
    })
}
