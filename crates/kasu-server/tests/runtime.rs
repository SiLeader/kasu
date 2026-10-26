use kasu_core::RepoPath;
use kasu_proto::v1::{self as p, metadata_client::MetadataClient, objects_client::ObjectsClient};
use kasu_server::*;
use kasu_store::*;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tonic::{Code, Request, transport::Channel};

struct Fixture {
    _temp: tempfile::TempDir,
    state: Arc<Backend>,
    meta: Arc<SqliteStore>,
    blobs: Arc<FsBlobStore>,
    http: String,
    channel: Channel,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixture {
    async fn new(limits: ServerLimits) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let meta = Arc::new(SqliteStore::memory().unwrap());
        let blobs = Arc::new(FsBlobStore::open(temp.path()).unwrap());
        let state = Arc::new(
            Backend::new(
                b"repo".to_vec(),
                meta.clone(),
                blobs.clone(),
                meta.clone(),
                Some("operator-secret".into()),
            )
            .with_object_token("hop-secret".into())
            .unwrap()
            .with_projection_accounts(vec![ProjectionAccount {
                name: "reader".into(),
                token: "reader-secret".into(),
                directories: vec![RepoPath::new(b"deploy".as_slice()).unwrap()],
            }])
            .unwrap()
            .with_limits(limits)
            .unwrap(),
        );
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_address = format!("http://{}", http.local_addr().unwrap());
        let grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_address = format!("http://{}", grpc.local_addr().unwrap());
        let hs = state.clone();
        let gs = state.clone();
        let tasks = vec![
            tokio::spawn(async move {
                axum::serve(
                    http,
                    http_router(hs).into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .unwrap();
            }),
            tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(MetadataService::new(gs.clone()).into_server())
                    .add_service(WriteService::new(gs.clone()).into_server())
                    .add_service(ObjectService::new(gs).into_server())
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(grpc))
                    .await
                    .unwrap();
            }),
        ];
        let channel = Channel::from_shared(grpc_address)
            .unwrap()
            .connect()
            .await
            .unwrap();
        Self {
            _temp: temp,
            state,
            meta,
            blobs,
            http: http_address,
            channel,
            tasks,
        }
    }
    fn refs(&self, client: &reqwest::Client) -> reqwest::RequestBuilder {
        client
            .post(format!("{}/v1/refs", self.http))
            .header("content-type", "application/json")
            .body(
                serde_json::to_vec(&p::ListRefsRequest {
                    repo: b"repo".to_vec(),
                    prefix: vec![],
                })
                .unwrap(),
            )
    }
}
fn authorized<T>(message: T, token: &str) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}
fn list_refs() -> p::ListRefsRequest {
    p::ListRefsRequest {
        repo: b"repo".to_vec(),
        prefix: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_basic_and_both_transports_share_one_token_budget() {
    let f = Fixture::new(ServerLimits {
        token_burst: 2,
        token_rate_per_second: 1,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let r = f
        .refs(&client)
        .bearer_auth("operator-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let r = f
        .refs(&client)
        .basic_auth("different-username", Some("operator-secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let mut grpc = MetadataClient::new(f.channel.clone());
    assert_eq!(
        grpc.list_refs(authorized(list_refs(), "operator-secret"))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    // Separate authenticated principals have separate budgets on the same socket IP.
    let r = client
        .get(format!(
            "{}/repo/-/path/deploy.git/info/refs?service=git-upload-pack",
            f.http
        ))
        .basic_auth("reader", Some("reader-secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let mut objects = ObjectsClient::new(f.channel.clone());
    // Empty batch is valid; hop credential is independent of the exhausted operator.
    let mut stream = objects
        .get_blobs(authorized(
            p::ObjectRequest {
                repo: b"repo".to_vec(),
                ids: vec![],
            },
            "hop-secret",
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(stream.message().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let text = f.state.metrics();
            if text.contains(
                "kasu_server_results_total{transport=\"grpc\",method=\"GetBlobs\",code=\"ok\"} 1\n",
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let text = f.state.metrics();
    assert!(text.contains(
        "kasu_server_results_total{transport=\"http\",method=\"ListRefs\",code=\"ok\"} 2\n"
    ));
    assert!(text.contains("kasu_server_results_total{transport=\"grpc\",method=\"ListRefs\",code=\"resource_exhausted\"} 1\n"));
    for secret in [
        "operator-secret",
        "reader-secret",
        "hop-secret",
        "127.0.0.1",
        "deploy",
    ] {
        assert!(!text.contains(secret));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_socket_ip_budget_spans_http_grpc_and_ignores_forwarding_headers() {
    let f = Fixture::new(ServerLimits {
        client_burst: 2,
        client_rate_per_second: 1,
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    for ip in ["10.0.0.1", "10.0.0.2"] {
        let r = f
            .refs(&client)
            .header("x-forwarded-for", ip)
            .header("forwarded", format!("for={ip}"))
            .bearer_auth("operator-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
    }
    let mut grpc = MetadataClient::new(f.channel.clone());
    assert_eq!(
        grpc.list_refs(authorized(list_refs(), "operator-secret"))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    let text = f.state.metrics();
    assert!(text.contains("kasu_server_client_entries 1\n"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_require_repository_access_and_unknown_paths_have_bounded_labels() {
    let f = Fixture::new(ServerLimits::default()).await;
    let client = reqwest::Client::new();
    let url = format!("{}/metrics", f.http);
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("reader-secret")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("hop-secret")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    for n in 0..25 {
        assert_eq!(
            client
                .get(format!("{}/random-secret-path-{n}", f.http))
                .bearer_auth("operator-secret")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    let response = client
        .get(&url)
        .bearer_auth("operator-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    let text = response.text().await.unwrap();
    assert!(
        text.contains("kasu_server_requests_total{transport=\"http\",method=\"Unknown\"} 25\n")
    );
    assert!(!text.contains("random-secret-path"));
    assert!(text.contains("kasu_derived_cache_hits_total{name=\"RootTree\",version=\"1\"} 0\n"));
    assert!(text.contains("kasu_server_ref_cas_total{operation=\"land\",result=\"mismatch\"} 0\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_binary_loads_limits_and_rejects_invalid_configuration_before_listening() {
    use std::process::{Command, Stdio};
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("limits.json");
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kasu-server"));
        command
            .arg(temp.path().join("data"))
            .args(["repo", "127.0.0.1:0", "127.0.0.1:0"])
            .env("KASU_TOKEN", "binary-secret")
            .env("KASU_SERVER_LIMITS_FILE", &config)
            .env("RUST_LOG", "info");
        for name in [
            "KASU_PROJECTION_ACCOUNTS_FILE",
            "KASU_OBJECT_TOKEN_FILE",
            "KASU_WRITE_POLICY_FILE",
            "KASU_WEBHOOKS_FILE",
            "KASU_POSTGRES_DSN_FILE",
            "KASU_S3_BUCKET",
        ] {
            command.env_remove(name);
        }
        command
    };
    std::fs::write(&config, b"{\"active\":0}").unwrap();
    let invalid = command().output().unwrap();
    assert!(!invalid.status.success());
    let log = String::from_utf8(invalid.stderr).unwrap();
    assert!(log.contains("invalid server admission limits"));
    assert!(!log.contains("server listening"));
    std::fs::write(
        &config,
        b"{\"client_burst\":1,\"client_rate_per_second\":1}",
    )
    .unwrap();
    let log = temp.path().join("server.log");
    let mut child = Child(
        command()
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let address = loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&log).unwrap()
        );
        let text = std::fs::read_to_string(&log).unwrap();
        if let Some(address) = text
            .split("http=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
        {
            break address.to_owned();
        }
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let client = reqwest::Client::new();
    let url = format!("http://{address}/metrics");
    let response = client
        .get(&url)
        .bearer_auth("binary-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let metrics = response.text().await.unwrap();
    assert!(metrics.contains("kasu_server_client_entries 1\n"));
    let response = client
        .get(&url)
        .bearer_auth("binary-secret")
        .header("x-forwarded-for", "10.1.2.3")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn object_stream_backpressure_holds_admission_until_client_drops_stream() {
    let f = Fixture::new(ServerLimits {
        active: 1,
        per_identity_active: 1,
        queue_timeout_ms: 20,
        ..Default::default()
    })
    .await;
    let repo = Repository {
        meta: f.meta.as_ref(),
        blobs: f.blobs.as_ref(),
    };
    let mut random = 42u64;
    let bytes: Vec<_> = (0..8 * 1024 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random as u8
        })
        .collect();
    let blob = repo.put_blob(&bytes).unwrap();
    repo.associate_blob(b"repo", blob).unwrap();
    let mut objects = ObjectsClient::new(f.channel.clone());
    let mut stream = objects
        .get_blobs(authorized(
            p::ObjectRequest {
                repo: b"repo".to_vec(),
                ids: vec![blob.0.to_vec()],
            },
            "hop-secret",
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!stream.message().await.unwrap().unwrap().last);
    // Keep the response alive without reading the remaining compressed chunks.
    let mut metadata = MetadataClient::new(f.channel.clone());
    assert_eq!(
        metadata
            .list_refs(authorized(list_refs(), "operator-secret"))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    assert!(f.state.metrics().contains("kasu_server_active 1\n"));
    drop(stream);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if f.state.metrics().contains("kasu_server_active 0\n") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    metadata
        .list_refs(authorized(list_refs(), "operator-secret"))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_pack_backpressure_holds_admission_until_client_drops_response() {
    use kasu_core::*;
    use kasu_derived::{Ctx, Engine};
    use kasu_git::{GitCommit, ProjectionId, register_git, write_packet};
    let f = Fixture::new(ServerLimits {
        active: 1,
        per_identity_active: 1,
        queue_timeout_ms: 20,
        ..Default::default()
    })
    .await;
    let repo = Repository {
        meta: f.meta.as_ref(),
        blobs: f.blobs.as_ref(),
    };
    let mut random = 91u64;
    let bytes: Vec<_> = (0..8 * 1024 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random as u8
        })
        .collect();
    let blob = repo.put_blob(&bytes).unwrap();
    let sig = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: 1,
        tz: b"+0000".as_slice().into(),
    };
    let id = repo
        .put_changeset(&Changeset {
            parents: vec![],
            author: sig.clone(),
            committer: sig,
            message: b"large".as_slice().into(),
            changes: vec![FileChange::Set {
                path: RepoPath::new(b"data".as_slice()).unwrap(),
                entry: Entry::File {
                    blob,
                    executable: false,
                },
            }],
            git_provenance: None,
        })
        .unwrap();
    f.meta
        .compare_and_swap(
            b"repo",
            &[RefUpdate {
                name: b"refs/heads/main".to_vec(),
                expected: RefState::default(),
                new: Some(id),
            }],
            b"test",
            0,
        )
        .unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let oid = engine
        .derive::<GitCommit>(&Ctx { repo }, id, &ProjectionId::full(b"repo".as_slice()))
        .unwrap()
        .0;
    let mut body = vec![];
    write_packet(&mut body, b"command=fetch\n").unwrap();
    body.extend(b"0001");
    write_packet(&mut body, format!("want {oid}\n").as_bytes()).unwrap();
    write_packet(&mut body, b"done\n").unwrap();
    body.extend(b"0000");
    let client = reqwest::Client::new();
    let mut response = client
        .post(format!("{}/repo.git/git-upload-pack", f.http))
        .bearer_auth("operator-secret")
        .header("git-protocol", "version=2")
        .header("content-type", "application/x-git-upload-pack-request")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.chunk().await.unwrap().is_some());
    let mut metadata = MetadataClient::new(f.channel.clone());
    assert_eq!(
        metadata
            .list_refs(authorized(list_refs(), "operator-secret"))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    assert!(f.state.metrics().contains("kasu_server_active 1\n"));
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if f.state.metrics().contains("kasu_server_active 0\n") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    metadata
        .list_refs(authorized(list_refs(), "operator-secret"))
        .await
        .unwrap();
}
