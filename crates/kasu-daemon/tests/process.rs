use kasu_cache::{DiskStore, Key, Store};
use kasu_core::ObjectId;
use kasu_object::{Client, Kind, Layer};
use std::{
    io::BufRead,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
struct Process {
    child: Child,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    observed: Vec<String>,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn start(root: &Path, snapshot: Option<&Path>, upstream: &str, socket: &Path) -> Process {
    start_config(root, snapshot, upstream, socket, &[])
}
fn start_config(
    root: &Path,
    snapshot: Option<&Path>,
    upstream: &str,
    socket: &Path,
    extra: &[&str],
) -> Process {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kasu-daemon"));
    command.env("RUST_LOG", "info");
    command
        .args([
            "serve",
            "--repo",
            "repo",
            "--upstream",
            upstream,
            "--upstream-timeout-ms",
            "100",
            "--snapshot-interval-secs",
            "1",
        ])
        .arg("--data")
        .arg(root.join("data"))
        .arg("--socket")
        .arg(socket)
        .arg("--token-file")
        .arg(root.join("user-token"))
        .arg("--upstream-token-file")
        .arg(root.join("hop-token"));
    if let Some(snapshot) = snapshot {
        command.arg("--snapshot").arg(snapshot);
    }
    command.args(extra);
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, lines) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    Process {
        child,
        lines,
        observed: Vec::new(),
    }
}
async fn ready(process: &mut Process) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = process.lines.recv().await {
            process.observed.push(line.clone());
            if line.contains("kasu-daemon listening") {
                return;
            }
        }
        panic!("daemon exited before readiness");
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn executable_imports_offline_snapshots_but_fails_closed_and_reclaims_only_stale_sockets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let run = root.join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.join("user-token"), b"user\n").unwrap();
    std::fs::write(root.join("hop-token"), b"daemon-hop\n").unwrap();
    let source = DiskStore::open(&root.join("source")).unwrap();
    let raw = b"offline daemon frame";
    let key = Key {
        kind: Kind::Blob,
        id: ObjectId::hash(raw),
    };
    let frame = kasu_store::compress(raw).unwrap();
    source.put(key, Arc::new(frame.clone())).unwrap();
    source.add_member(b"repo", key).unwrap();
    let snapshot = root.join("seed.tar");
    source.export_snapshot(&snapshot).unwrap();
    let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", unavailable.local_addr().unwrap());
    drop(unavailable);
    let socket = run.join("node.sock");
    let endpoint = format!("unix://{}", socket.display());
    let mut first = start(root, Some(&snapshot), &upstream, &socket);
    ready(&mut first).await;
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let client = Client::connect(&endpoint, Some("user"), Some(Layer::Daemon))
        .await
        .unwrap();
    assert_eq!(
        client
            .fetch(b"repo", key.id, key.kind)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    assert!(
        client
            .fetch(b"repo", ObjectId([77; 32]), Kind::Blob)
            .await
            .is_err()
    );
    let mut duplicate = start(root, None, &upstream, &run.join("other.sock"));
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = duplicate.child.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!status.success());
    assert!(!run.join("other.sock").exists());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_dir(root.join("data/snapshots"))
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "tar")
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let exported = root.join("live-export.tar");
    let data = root.join("data");
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_kasu-daemon"))
            .args(["snapshot", "--repo", "repo"])
            .arg("--data")
            .arg(data)
            .arg(exported)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    drop(client);
    assert!(socket.exists());
    let mut restarted = start(root, None, &upstream, &socket);
    ready(&mut restarted).await;
    let client = Client::connect(&endpoint, Some("user"), Some(Layer::Daemon))
        .await
        .unwrap();
    assert_eq!(
        client
            .fetch(b"repo", key.id, key.kind)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unavailable
    );
    drop(client);
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(restarted.child.id() as i32).unwrap(),
        rustix::process::Signal::INT,
    )
    .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = restarted.child.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(status.success());
    assert!(!socket.exists());
}

#[tokio::test]
async fn socket_lease_preserves_non_socket_files_and_requires_a_private_directory() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("node.sock");
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(kasu_daemon::SocketLease::bind(&path).is_err());
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&path, b"user file").unwrap();
    assert!(kasu_daemon::SocketLease::bind(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"user file");
    let path = temp.path().join("replacement.sock");
    let (_listener, lease) = kasu_daemon::SocketLease::bind(&path).unwrap();
    assert!(kasu_daemon::SocketLease::bind(&path).is_err());
    std::fs::remove_file(&path).unwrap();
    let _replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(lease);
    assert!(path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_disk_refills_after_eviction_survives_restart_and_retains_only_managed_snapshots() {
    use kasu_proto::v1::{
        self as p,
        objects_server::{Objects, ObjectsServer},
    };
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicU64, Ordering},
    };
    use tonic::{Request, Response, Status};
    type Chunks = tokio_stream::Iter<std::vec::IntoIter<Result<p::BlobChunk, Status>>>;
    struct Source {
        frames: BTreeMap<Vec<u8>, Vec<u8>>,
        requests: Arc<AtomicU64>,
    }
    #[tonic::async_trait]
    impl Objects for Source {
        type GetTreesStream = tokio_stream::Iter<std::vec::IntoIter<Result<p::TreeObject, Status>>>;
        type GetBlobsStream = Chunks;
        async fn get_trees(
            &self,
            _: Request<p::ObjectRequest>,
        ) -> Result<Response<Self::GetTreesStream>, Status> {
            Err(Status::unimplemented("blobs only fixture"))
        }
        async fn get_blobs(
            &self,
            q: Request<p::ObjectRequest>,
        ) -> Result<Response<Chunks>, Status> {
            if q.metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_none_or(str::is_empty)
            {
                return Err(Status::unauthenticated("hop required"));
            }
            let input = q.into_inner();
            if input.repo != b"repo" {
                return Err(Status::permission_denied("wrong repo"));
            }
            self.requests.fetch_add(1, Ordering::Relaxed);
            let mut chunks = vec![];
            for id in input.ids {
                let data = self
                    .frames
                    .get(&id)
                    .ok_or_else(|| Status::not_found("missing frame"))?;
                chunks.push(Ok(p::BlobChunk {
                    id,
                    offset: 0,
                    data: data.clone(),
                    last: true,
                }));
            }
            let mut response = Response::new(tokio_stream::iter(chunks));
            response
                .metadata_mut()
                .insert("x-kasu-object-layer", "cache".parse().unwrap());
            Ok(response)
        }
        async fn get_repository_identity(
            &self,
            q: Request<p::RepositoryIdentityRequest>,
        ) -> Result<Response<p::RepositoryIdentityResponse>, Status> {
            let authorization = q
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok());
            if authorization.is_none_or(str::is_empty) {
                return Err(Status::unauthenticated("hop required"));
            }
            if q.into_inner().repo != b"repo" {
                return Err(Status::permission_denied("wrong repo"));
            }
            let mut response = Response::new(p::RepositoryIdentityResponse {
                identity: "fixture-repository".into(),
            });
            response
                .metadata_mut()
                .insert("x-kasu-object-layer", "cache".parse().unwrap());
            Ok(response)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let run = root.join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.join("user-token"), b"user").unwrap();
    std::fs::write(root.join("hop-token"), b"daemon-hop").unwrap();
    let mut frames = BTreeMap::new();
    let mut ids = vec![];
    for raw in [
        b"first cached blob".to_vec(),
        b"second cached blob".to_vec(),
        (0..4096u32).flat_map(u32::to_le_bytes).collect(),
    ] {
        let id = ObjectId::hash(&raw);
        ids.push(id);
        frames.insert(id.0.to_vec(), kasu_store::compress(&raw).unwrap());
    }
    assert!(frames[ids[2].0.as_slice()].len() > 4096);
    let requests = Arc::new(AtomicU64::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let source = Source {
        frames: frames.clone(),
        requests: requests.clone(),
    };
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ObjectsServer::new(source))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let socket = run.join("quota.sock");
    let endpoint = format!("unix://{}", socket.display());
    let extra = [
        "--disk-entries",
        "1",
        "--disk-bytes",
        "4096",
        "--snapshot-keep",
        "1",
        "--metrics",
        "127.0.0.1:0",
    ];
    let mut child = start_config(root, None, &upstream, &socket, &extra);
    ready(&mut child).await;
    let client = Client::connect(&endpoint, Some("user"), Some(Layer::Daemon))
        .await
        .unwrap();
    for id in &ids[..2] {
        assert_eq!(
            client.fetch(b"repo", *id, Kind::Blob).await.unwrap(),
            frames[id.0.as_slice()]
        );
    }
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    let data = root.join("data");
    let probe = DiskStore::open(&data).unwrap();
    assert_eq!(probe.limits().unwrap().entries, 1);
    assert!(
        probe
            .get(Key {
                kind: Kind::Blob,
                id: ids[0]
            })
            .unwrap()
            .is_none()
    );
    assert!(
        probe
            .get(Key {
                kind: Kind::Blob,
                id: ids[1]
            })
            .unwrap()
            .is_some()
    );
    // Keep an unrelated archive through several periodic retention cycles.
    std::fs::write(
        data.join("snapshots/operator.tar"),
        b"preserve operator backup",
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut saved = 0;
        while let Some(line) = child.lines.recv().await {
            if line.contains("daemon snapshot saved") {
                saved += 1;
                if saved == 3 {
                    return;
                }
            }
        }
        panic!("daemon stopped");
    })
    .await
    .unwrap();
    let managed: Vec<_> = std::fs::read_dir(data.join("snapshots"))
        .unwrap()
        .filter_map(|e| {
            let e = e.unwrap();
            e.file_name()
                .to_str()
                .filter(|n| n.starts_with("kasu-snapshot-v1-"))
                .map(|_| e.path())
        })
        .collect();
    assert_eq!(managed.len(), 1);
    assert_eq!(
        std::fs::read(data.join("snapshots/operator.tar")).unwrap(),
        b"preserve operator backup"
    );
    child.child.kill().unwrap();
    child.child.wait().unwrap();
    drop(client);
    let mut restarted = start_config(root, None, &upstream, &socket, &extra);
    ready(&mut restarted).await;
    let client = Client::connect(&endpoint, Some("user"), Some(Layer::Daemon))
        .await
        .unwrap();
    assert_eq!(
        client.fetch(b"repo", ids[1], Kind::Blob).await.unwrap(),
        frames[ids[1].0.as_slice()]
    );
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    assert_eq!(
        client.fetch(b"repo", ids[0], Kind::Blob).await.unwrap(),
        frames[ids[0].0.as_slice()]
    );
    assert_eq!(requests.load(Ordering::Relaxed), 3);
    for _ in 0..2 {
        assert_eq!(
            client.fetch(b"repo", ids[2], Kind::Blob).await.unwrap(),
            frames[ids[2].0.as_slice()]
        );
    }
    assert_eq!(requests.load(Ordering::Relaxed), 5);
    assert!(
        probe
            .get(Key {
                kind: Kind::Blob,
                id: ids[0]
            })
            .unwrap()
            .is_some()
    );
    assert!(
        probe
            .get(Key {
                kind: Kind::Blob,
                id: ids[2]
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(probe.stats().unwrap().admission_skips, 2);
    assert!(probe.stats().unwrap().bytes <= 4096);
    let address = restarted
        .observed
        .iter()
        .find_map(|line| {
            line.split("metrics=")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
        })
        .unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    tcp.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = vec![];
    tokio::time::timeout(Duration::from_secs(2), tcp.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let metrics = String::from_utf8(response).unwrap();
    assert!(metrics.contains("kasu_object_disk_limit_entries{layer=\"daemon\"} 1\n"));
    assert!(metrics.contains("kasu_object_disk_admission_skips_total{layer=\"daemon\"} 2\n"));
    if let Some(path) = std::env::var_os("KASU_DISK_CAPTURE") {
        let path = std::path::Path::new(&path);
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("daemon-quota-metrics.http"), metrics).unwrap();
        std::fs::write(
            path.join("daemon-quota.json"),
            serde_fixture(
                requests.load(Ordering::Relaxed),
                probe.stats().unwrap().bytes,
                managed.len(),
            ),
        )
        .unwrap();
    }
    server.abort();
}
fn serde_fixture(requests: u64, bytes: u64, snapshots: usize) -> String {
    format!(
        "{{\"upstream_requests\":{requests},\"entries\":1,\"bytes\":{bytes},\"admission_skips\":2,\"retained_snapshots\":{snapshots},\"operator_archive_preserved\":true,\"restart_warm_hit\":true}}\n"
    )
}
