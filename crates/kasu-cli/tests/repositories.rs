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
            .env_remove("KASU_OBJECTS_ENDPOINT")
            .env_remove("KASU_SERVER")
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
async fn repo_commands_and_clone_work_across_repositories() {
    let temp = tempfile::tempdir().unwrap();
    let meta = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
    let backend = Arc::new(
        Backend::with_registry(
            meta.clone(),
            blobs,
            meta.clone(),
            meta.clone(),
            Some("token".into()),
        )
        .with_registry_ttl(std::time::Duration::from_millis(1)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let service = backend.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(MetadataService::new(service.clone()).into_server())
            .add_service(WriteService::new(service.clone()).into_server())
            .add_service(ObjectService::new(service.clone()).into_server())
            .add_service(AdminService::new(service).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let dir = temp.path();
    let args = |extra: &[&str]| -> Vec<String> {
        ["repo", "--server", &url]
            .iter()
            .chain(extra)
            .map(|s| s.to_string())
            .collect()
    };
    ok(&kasu(
        dir,
        args(&["create", "team/app", "--default-branch", "trunk"]),
    )
    .await);
    ok(&kasu(dir, args(&["create", "team/lib"])).await);
    let listed = ok(&kasu(dir, args(&["list", "--prefix", "team/"])).await);
    assert_eq!(listed, "team/app\tACTIVE\ttrunk\nteam/lib\tACTIVE\tmain\n");
    assert!(ok(&kasu(dir, args(&["show", "team/lib"])).await).starts_with("team/lib\t"));
    assert!(
        !kasu(dir, args(&["show", "team/none"]))
            .await
            .status
            .success()
    );
    assert!(
        !kasu(dir, args(&["create", "team/app"]))
            .await
            .status
            .success()
    );

    // Each repository gets its own commit on its own default branch.
    for (repo, branch, content) in [
        (
            "team/app",
            b"refs/heads/trunk".as_slice(),
            b"app".as_slice(),
        ),
        ("team/lib", b"refs/heads/main", b"lib"),
    ] {
        let state = backend
            .scope(Some("Bearer token"), repo.as_bytes(), Mode::Write)
            .unwrap();
        let blob = ObjectId::hash(content).0.to_vec();
        let ops = Repository {
            meta: meta.as_ref(),
            blobs: &FsBlobStore::open(temp.path().join("objects")).unwrap(),
        };
        let id = ops.put_blob(content).unwrap();
        let resolved = meta
            .resolve_repository_name(&RepoName::new(repo).unwrap(), 0)
            .unwrap()
            .unwrap()
            .key;
        ops.associate_blob(resolved.as_bytes(), id).unwrap();
        let signature = p::Signature {
            name: b"A".to_vec(),
            email: b"a@b".to_vec(),
            timestamp: 1,
            tz: b"+0000".to_vec(),
        };
        let changeset = state
            .create_changeset(p::CreateChangesetRequest {
                repo: repo.as_bytes().to_vec(),
                parents: vec![],
                changes: vec![p::FileChange {
                    path: b"README".to_vec(),
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
                    message: b"init".to_vec(),
                }),
            })
            .unwrap();
        let updated = state
            .update_ref(p::UpdateRefRequest {
                repo: repo.as_bytes().to_vec(),
                name: branch.to_vec(),
                expected: Some(p::Ref {
                    name: branch.to_vec(),
                    target: None,
                    version: 0,
                }),
                target: Some(changeset.id),
                force: false,
            })
            .unwrap();
        assert!(updated.applied);
    }
    // Clone uses the registered default branch and the last name segment.
    let cloned = |repo: &str| {
        let url = url.clone();
        let repo = repo.to_owned();
        async move { kasu(dir, vec!["clone".into(), url, repo]).await }
    };
    ok(&cloned("team/app").await);
    ok(&cloned("team/lib").await);
    assert_eq!(std::fs::read(dir.join("app/README")).unwrap(), b"app");
    assert_eq!(std::fs::read(dir.join("lib/README")).unwrap(), b"lib");
    // The working copy remembers its repository for later commands.
    let log = ok(&kasu(&dir.join("app"), vec!["log".into()]).await);
    assert!(log.contains("init"), "{log}");
    assert!(!cloned("team/none").await.status.success());
    server.abort();
}
