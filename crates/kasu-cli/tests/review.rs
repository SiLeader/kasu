use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::{path::Path, process::Command, sync::Arc};

async fn kasu(directory: &Path, args: Vec<String>) -> std::process::Output {
    let directory = directory.to_owned();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_kasu"))
            .arg("-C")
            .arg(directory)
            .args(args)
            .env("KASU_TOKEN", "token")
            .env_remove("KASU_SERVER")
            .env_remove("KASU_REPO")
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}
fn ok(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_commands_create_inspect_and_submit() {
    let temp = tempfile::tempdir().unwrap();
    let meta = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
    let record = RepositoryRecord::new(
        RepoKey::generate().unwrap(),
        RepoName::new("app").unwrap(),
        b"test",
        1,
    );
    meta.create_repository(&record).unwrap();
    let backend = Arc::new(
        Backend::with_registry(
            meta.clone(),
            blobs.clone(),
            meta.clone(),
            meta.clone(),
            Some("token".into()),
        )
        .with_registry_ttl(std::time::Duration::from_millis(1))
        .with_reviews(meta.clone()),
    );
    let state = backend
        .scope(Some("Bearer token"), b"app", Mode::Write)
        .unwrap();
    let repository = Repository {
        meta: meta.as_ref(),
        blobs: blobs.as_ref(),
    };
    let signature = p::Signature {
        name: b"A".to_vec(),
        email: b"a@b".to_vec(),
        timestamp: 1,
        tz: b"+0000".to_vec(),
    };
    let mut parent: Option<Vec<u8>> = None;
    let mut ids = Vec::new();
    for content in [b"base".as_slice(), b"change"] {
        let blob = repository.put_blob(content).unwrap();
        repository
            .associate_blob(record.key.as_bytes(), blob)
            .unwrap();
        let cs = state
            .create_changeset(p::CreateChangesetRequest {
                repo: b"app".to_vec(),
                parents: parent.iter().cloned().collect(),
                changes: vec![p::FileChange {
                    path: b"f".to_vec(),
                    entry: Some(p::Entry {
                        kind: Some(p::entry::Kind::File(p::File {
                            blob: blob.0.to_vec(),
                            executable: false,
                        })),
                    }),
                }],
                metadata: Some(p::CommitMetadata {
                    author: Some(signature.clone()),
                    committer: Some(signature.clone()),
                    message: content.to_vec(),
                }),
            })
            .unwrap();
        parent = Some(cs.id.clone());
        ids.push(cs.id);
    }
    state
        .update_ref(p::UpdateRefRequest {
            repo: b"app".to_vec(),
            name: b"refs/heads/main".to_vec(),
            expected: Some(p::Ref {
                name: b"refs/heads/main".to_vec(),
                target: None,
                version: 0,
            }),
            target: Some(ids[0].clone()),
            force: false,
        })
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let service = backend.clone();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ReviewService::new(service).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let dir = temp.path();
    let args = |extra: &[&str]| -> Vec<String> {
        ["review", "--server", &url, "--repo", "app"]
            .iter()
            .chain(extra)
            .map(|s| s.to_string())
            .collect()
    };
    let head = hex::encode(&ids[1]);
    let created = ok(&kasu(dir, args(&["create", "--title", "Change f", &head])).await);
    assert_eq!(created, "#1\tOPEN\trefs/heads/main\toperator\tChange f\n");
    let shown = ok(&kasu(dir, args(&["show", "1"])).await);
    assert!(
        shown.contains(&format!("revision 1\toperator\t{head}")),
        "{shown}"
    );
    assert!(shown.contains("submit\tready\t0/0 approvals"), "{shown}");
    ok(&kasu(dir, args(&["comment", "1", "looks fine"])).await);
    let submitted = ok(&kasu(dir, args(&["submit", "1"])).await);
    assert!(submitted.starts_with("Landed "), "{submitted}");
    let listed = ok(&kasu(dir, args(&["list", "--state", "submitted"])).await);
    assert!(listed.starts_with("#1\tSUBMITTED"), "{listed}");
    assert!(!kasu(dir, args(&["submit", "1"])).await.status.success());
}
