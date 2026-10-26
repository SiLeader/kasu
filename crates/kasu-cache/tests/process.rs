use kasu_core::ObjectId;
use kasu_object::{Client, Kind, Layer};
use kasu_server::{Backend, ObjectService};
use kasu_store::{BlobStore, FsBlobStore, Repository, SqliteStore};
use std::{
    io::BufRead,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_executable_serves_only_objects_exposes_metrics_and_uses_its_hop_token() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let meta = Arc::new(SqliteStore::open(root.join("meta.db")).unwrap());
    let blobs = Arc::new(FsBlobStore::open(root.join("objects")).unwrap());
    let repo = Repository {
        meta: meta.as_ref(),
        blobs: blobs.as_ref(),
    };
    let id = repo.put_blob(b"cache process frame").unwrap();
    repo.associate_blob(b"repo", id).unwrap();
    let expected = blobs.get_compressed(ObjectId(id.0)).unwrap();
    let state = Arc::new(
        Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            blobs,
            meta,
            Some("user".into()),
        )
        .with_object_token("server-hop".into())
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let _server = Server(tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ObjectService::new(state).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    }));
    std::fs::write(root.join("downstream"), b"daemon-hop\n").unwrap();
    std::fs::write(root.join("upstream"), b"server-hop\n").unwrap();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_kasu-cache"))
            .args([
                "--repo",
                "repo",
                "--upstream",
                &upstream,
                "--listen",
                "127.0.0.1:0",
                "--metrics",
                "127.0.0.1:0",
            ])
            .arg("--token-file")
            .arg(root.join("downstream"))
            .arg("--upstream-token-file")
            .arg(root.join("upstream"))
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = child.0.stderr.take().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
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
    let (address, metrics) = tokio::time::timeout(Duration::from_secs(10), async {
        let mut metrics = None;
        while let Some(line) = rx.recv().await {
            for token in line.split_whitespace() {
                if let Some(value) = token.strip_prefix("metrics=") {
                    metrics = Some(value.to_owned());
                }
                if let Some(value) = token.strip_prefix("address=") {
                    return (value.to_owned(), metrics.unwrap());
                }
            }
        }
        panic!("cache exited before readiness")
    })
    .await
    .unwrap();
    let endpoint = format!("http://{address}");
    let client = Client::connect(&endpoint, Some("daemon-hop"), Some(Layer::Cache))
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            client
                .fetch(b"repo", ObjectId(id.0), Kind::Blob)
                .await
                .unwrap(),
            expected
        );
    }
    let wrong = Client::connect(&endpoint, Some("user"), Some(Layer::Cache))
        .await
        .unwrap();
    assert_eq!(
        wrong
            .fetch(b"repo", ObjectId(id.0), Kind::Blob)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    assert_eq!(
        client
            .fetch(b"foreign", ObjectId(id.0), Kind::Blob)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let mut stream = tokio::net::TcpStream::connect(&metrics).await.unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut bytes = vec![];
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let metrics = String::from_utf8(bytes).unwrap();
    assert!(metrics.contains("kasu_object_hits_total{layer=\"cache\"} 1"));
    assert!(metrics.contains("kasu_object_upstream_requests_total{layer=\"cache\"} 1"));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(3),
            client.fetch(b"repo", ObjectId(id.0), Kind::Blob)
        )
        .await
        .unwrap()
        .is_err()
    );
}
