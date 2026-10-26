#![cfg(feature = "production")]
use kasu_core::ObjectId;
use kasu_proto::v1::{self as p, metadata_client::MetadataClient, writer_client::WriterClient};
use kasu_store::*;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tonic::transport::Channel;
fn auth<T>(q: T) -> tonic::Request<T> {
    let mut q = tonic::Request::new(q);
    q.metadata_mut()
        .insert("authorization", "Bearer shared-test".parse().unwrap());
    q
}
fn git(path: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "storage test")
        .env("GIT_AUTHOR_EMAIL", "storage@kasu.invalid")
        .env("GIT_COMMITTER_NAME", "storage test")
        .env("GIT_COMMITTER_EMAIL", "storage@kasu.invalid")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}
struct Config {
    root: PathBuf,
    dsn: PathBuf,
    repo: String,
    prefix: String,
}
impl Config {
    fn command(&self, binary: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(binary);
        command
            .env("KASU_POSTGRES_DSN_FILE", &self.dsn)
            .env("KASU_POSTGRES_ALLOW_PLAINTEXT", "1")
            .env(
                "KASU_S3_BUCKET",
                std::env::var("KASU_TEST_S3_BUCKET").unwrap(),
            )
            .env(
                "KASU_S3_ENDPOINT",
                std::env::var("KASU_TEST_S3_ENDPOINT").unwrap(),
            )
            .env("KASU_S3_REGION", "us-east-1")
            .env("KASU_S3_ALLOW_PLAINTEXT", "1")
            .env("KASU_S3_PATH_STYLE", "1")
            .env("KASU_S3_PREFIX", &self.prefix)
            .env_remove("KASU_POSTGRES_CA_FILE")
            .env_remove("KASU_OBJECT_TOKEN_FILE")
            .env_remove("KASU_WEBHOOKS_FILE")
            .env_remove("KASU_WRITE_POLICY_FILE")
            .env("KASU_TOKEN", "shared-test")
            .env("RUST_LOG", "info");
        command
    }
}
struct Server {
    child: Child,
    grpc: String,
    http: String,
    log: PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    async fn start(config: &Config, label: &str) -> Self {
        Self::start_with(config, label, &[]).await
    }
    async fn start_with(config: &Config, label: &str, env: &[(&str, &str)]) -> Self {
        let grpc = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let grpc_address = grpc.local_addr().unwrap();
        let http_address = http.local_addr().unwrap();
        drop(grpc);
        drop(http);
        let log = config.root.join(format!("{label}.log"));
        let output = std::fs::File::create(&log).unwrap();
        let child = config
            .command(env!("CARGO_BIN_EXE_kasu-server"))
            .arg(config.root.join(format!("unused-{label}")))
            .arg(&config.repo)
            .arg(grpc_address.to_string())
            .arg(http_address.to_string())
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .stderr(output)
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            grpc: format!("http://{grpc_address}"),
            http: format!("http://{http_address}/{}.git", config.repo),
            log,
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let log = std::fs::read_to_string(&server.log).unwrap();
            if log.contains("kasu metadata server listening") {
                break;
            }
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "server stopped: {log}"
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "server readiness timeout: {log}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server
    }
    async fn metadata(&self) -> MetadataClient<Channel> {
        MetadataClient::connect(self.grpc.clone()).await.unwrap()
    }
    async fn writer(&self) -> WriterClient<Channel> {
        WriterClient::connect(self.grpc.clone()).await.unwrap()
    }
}
async fn current(client: &mut MetadataClient<Channel>, repo: &[u8]) -> p::Ref {
    client
        .resolve_ref(auth(p::RefRequest {
            repo: repo.to_vec(),
            name: b"refs/heads/main".to_vec(),
        }))
        .await
        .unwrap()
        .into_inner()
}
async fn create(
    client: &mut WriterClient<Channel>,
    repo: &[u8],
    parent: Vec<u8>,
    path: &[u8],
    content: &[u8],
) -> Vec<u8> {
    let blob = ObjectId::hash(content).0.to_vec();
    client
        .upload_blobs(auth(tokio_stream::iter([p::BlobUpload {
            repo: repo.to_vec(),
            id: blob.clone(),
            offset: 0,
            data: content.to_vec(),
            last: true,
        }])))
        .await
        .unwrap();
    let signature = p::Signature {
        name: b"writer".to_vec(),
        email: b"writer@kasu.invalid".to_vec(),
        timestamp: 1700000100,
        tz: b"+0000".to_vec(),
    };
    client
        .create_changeset(auth(p::CreateChangesetRequest {
            repo: repo.to_vec(),
            parents: vec![parent],
            changes: vec![p::FileChange {
                path: path.to_vec(),
                entry: Some(p::Entry {
                    kind: Some(p::entry::Kind::File(p::File {
                        blob,
                        executable: false,
                    })),
                }),
            }],
            metadata: Some(p::CommitMetadata {
                author: Some(signature.clone()),
                committer: Some(signature),
                message: path.to_vec(),
            }),
        }))
        .await
        .unwrap()
        .into_inner()
        .id
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL/S3 env and KASU_TEST_INGEST_BIN built with production"]
async fn two_server_processes_share_ingest_git_atomic_push_native_cas_and_land_replay() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let dsn = root.join("postgres-dsn");
    std::fs::write(&dsn, std::env::var("KASU_TEST_POSTGRES").unwrap()).unwrap();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let config = Config {
        root: root.into(),
        dsn,
        repo: format!("shared-{stamp}"),
        prefix: format!("shared/{stamp}"),
    };
    let repo = config.repo.as_bytes();
    let source = root.join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "--template=", "--initial-branch=main"]);
    std::fs::write(source.join("f"), b"initial\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-qm", "initial"]);
    let mut ingest =
        config.command(std::env::var_os("KASU_TEST_INGEST_BIN").expect("KASU_TEST_INGEST_BIN"));
    ingest
        .arg(&source)
        .arg(root.join("unused-import"))
        .arg(&config.repo);
    let result = tokio::task::spawn_blocking(move || ingest.output().unwrap())
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!root.join("unused-import").exists());
    let mut a = Server::start(&config, "a").await;
    let b = Server::start(&config, "b").await;
    let mut am = a.metadata().await;
    let mut bm = b.metadata().await;
    let initial = current(&mut am, repo).await;
    assert_eq!(current(&mut bm, repo).await, initial);
    assert_eq!(
        bm.read_file(auth(p::PathRequest {
            follow_copies: false,
            follow_moves: false,
            repo: repo.to_vec(),
            id: initial.target.clone().unwrap(),
            path: b"f".to_vec()
        }))
        .await
        .unwrap()
        .into_inner()
        .content,
        b"initial\n"
    );
    std::fs::write(source.join("f"), b"git update\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-qm", "git update"]);
    git(&source, &["tag", "-a", "release", "-m", "release"]);
    let url = a.http.clone();
    let path = source.clone();
    tokio::task::spawn_blocking(move || {
        git(
            &path,
            &[
                "-c",
                "http.extraHeader=Authorization: Bearer shared-test",
                "push",
                "--atomic",
                &url,
                "main",
                "refs/tags/release",
            ],
        )
    })
    .await
    .unwrap();
    let after_git = current(&mut bm, repo).await;
    assert_ne!(after_git.target, initial.target);
    assert_eq!(current(&mut am, repo).await, after_git);
    let path = source.clone();
    let destination = root.join("mirror.git");
    let clone = destination.clone();
    let url = b.http.clone();
    tokio::task::spawn_blocking(move || {
        git(
            &path,
            &[
                "-c",
                "http.extraHeader=Authorization: Bearer shared-test",
                "clone",
                "--mirror",
                &url,
                clone.to_str().unwrap(),
            ],
        );
        git(&clone, &["fsck", "--strict"]);
    })
    .await
    .unwrap();
    assert_eq!(
        git(&destination, &["rev-parse", "refs/tags/release"]),
        git(&source, &["rev-parse", "refs/tags/release"])
    );
    let mut aw = a.writer().await;
    let mut bw = b.writer().await;
    let parent = after_git.target.clone().unwrap();
    let draft_a = create(&mut aw, repo, parent.clone(), b"native-a", b"from-a").await;
    let draft_b = create(&mut bw, repo, parent, b"native-b", b"from-b").await;
    assert_eq!(
        bm.read_file(auth(p::PathRequest {
            follow_copies: false,
            follow_moves: false,
            repo: repo.to_vec(),
            id: draft_a.clone(),
            path: b"native-a".to_vec()
        }))
        .await
        .unwrap()
        .into_inner()
        .content,
        b"from-a"
    );
    let objects = kasu_object::Client::connect(
        &b.grpc,
        Some("shared-test"),
        Some(kasu_object::Layer::Server),
    )
    .await
    .unwrap();
    let frame = objects
        .fetch(repo, ObjectId::hash(b"from-a"), kasu_object::Kind::Blob)
        .await
        .unwrap();
    assert_eq!(decompress(&frame).unwrap(), b"from-a");
    let mut pg_options = PostgresOptions::new(std::env::var("KASU_TEST_POSTGRES").unwrap());
    pg_options.allow_plaintext = true;
    let store = PostgresStore::connect(pg_options).unwrap();
    let before = store.tail(repo, 0, 100).unwrap().len();
    let request = |target| {
        auth(p::UpdateRefRequest {
            repo: repo.to_vec(),
            name: b"refs/heads/main".to_vec(),
            expected: Some(after_git.clone()),
            target: Some(target),
            force: false,
        })
    };
    let (ra, rb) = tokio::join!(
        aw.update_ref(request(draft_a.clone())),
        bw.update_ref(request(draft_b.clone()))
    );
    let ra = ra.unwrap().into_inner();
    let rb = rb.unwrap().into_inner();
    assert_ne!(ra.applied, rb.applied);
    assert_eq!(store.tail(repo, 0, 100).unwrap().len(), before + 1);
    let head = current(&mut bm, repo).await;
    assert_eq!(current(&mut am, repo).await, head);
    let draft = create(
        &mut aw,
        repo,
        head.target.clone().unwrap(),
        b"landed",
        b"once across servers",
    )
    .await;
    let q = p::LandRequest {
        required_ancestor: Vec::new(),
        repo: repo.to_vec(),
        id: draft,
        name: b"refs/heads/main".to_vec(),
        idempotency_key: b"shared-key".to_vec(),
        max_attempts: 3,
        timeout_ms: 5000,
    };
    let first = aw.land(auth(q.clone())).await.unwrap().into_inner();
    assert_eq!(first.outcome, p::LandOutcome::Landed as i32);
    assert!(!first.replayed);
    let replay = bw.land(auth(q.clone())).await.unwrap().into_inner();
    assert!(replay.replayed);
    assert_eq!(replay.id, first.id);
    assert_eq!(store.tail(repo, 0, 100).unwrap().len(), before + 2);
    let foreign = bm
        .get_changeset(auth(p::ChangesetRequest {
            repo: b"foreign".to_vec(),
            id: first.id.clone().unwrap(),
        }))
        .await
        .unwrap_err();
    assert_eq!(foreign.code(), tonic::Code::PermissionDenied);
    a.child.kill().unwrap();
    a.child.wait().unwrap();
    drop(am);
    drop(aw);
    drop(a);
    let restarted = Server::start(&config, "a-restarted").await;
    let replay = restarted
        .writer()
        .await
        .land(auth(q))
        .await
        .unwrap()
        .into_inner();
    assert!(replay.replayed);
    assert_eq!(replay.id, first.id);
    assert_eq!(store.tail(repo, 0, 100).unwrap().len(), before + 2);
    assert_eq!(
        current(&mut restarted.metadata().await, repo).await,
        current(&mut bm, repo).await
    );
    let denied = Server::start_with(
        &config,
        "bad-s3",
        &[
            ("AWS_ACCESS_KEY_ID", "invalid-test-key"),
            ("AWS_SECRET_ACCESS_KEY", "invalid-secret-marker"),
        ],
    )
    .await;
    let error = denied
        .metadata()
        .await
        .read_file(auth(p::PathRequest {
            follow_copies: false,
            follow_moves: false,
            repo: repo.to_vec(),
            id: first.id.unwrap(),
            path: b"landed".to_vec(),
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Internal);
    assert!(!error.message().contains("invalid-test-key"));
    assert!(!error.message().contains("invalid-secret-marker"));
    assert!(
        !std::fs::read_to_string(&denied.log)
            .unwrap()
            .contains("invalid-secret-marker")
    );
    assert!(!root.join("unused-bad-s3").exists());
    for name in ["unused-a", "unused-b", "unused-a-restarted"] {
        assert!(!root.join(name).exists());
    }
}
