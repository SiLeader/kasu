use kasu_core::*;
use kasu_derived::{Ctx, Engine};
use kasu_ops::Operations;
use kasu_proto::v1::{
    self as p,
    objects_server::{Objects, ObjectsServer},
};
use kasu_server::*;
use kasu_store::*;
use p::writer_server::{Writer, WriterServer};
use std::{
    collections::BTreeSet,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
};
use tonic::{Request, Response, Status};
struct LostLand {
    inner: WriteService,
    lose: std::sync::atomic::AtomicBool,
    retry: bool,
}
macro_rules! forward_writer {($(($method:ident,$input:ty,$output:ty)),*)=>{
    #[tonic::async_trait]
    impl Writer for LostLand {
        $(async fn $method(&self,q:Request<$input>)->std::result::Result<Response<$output>,Status>{self.inner.$method(q).await})*
        async fn land(&self,q:Request<p::LandRequest>)->std::result::Result<Response<p::LandResponse>,Status>{
            let response=self.inner.land(q).await?;
            if self.lose.swap(false,std::sync::atomic::Ordering::SeqCst){
                if self.retry {
                    Ok(Response::new(p::LandResponse {
                        outcome: p::LandOutcome::Retry as i32,
                        message: "injected lost Land response after commit".into(),
                        ..Default::default()
                    }))
                } else {
                    Err(Status::unavailable("injected lost Land response"))
                }
            }else{Ok(response)}
        }
    }
}}
forward_writer!(
    (
        upload_blobs,
        tonic::Streaming<p::BlobUpload>,
        p::UploadBlobsResponse
    ),
    (create_changeset, p::CreateChangesetRequest, p::Changeset),
    (merge, p::MergeRequest, p::RewriteResponse),
    (rebase, p::RebaseRequest, p::RewriteResponse),
    (update_ref, p::UpdateRefRequest, p::UpdateRefResponse)
);

type ReadLog = Arc<Mutex<Vec<(bool, Vec<u8>)>>>;
struct Observed {
    inner: ObjectService,
    reads: ReadLog,
}
#[tonic::async_trait]
impl Objects for Observed {
    type GetTreesStream = <ObjectService as Objects>::GetTreesStream;
    type GetBlobsStream = <ObjectService as Objects>::GetBlobsStream;
    async fn get_trees(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> std::result::Result<Response<Self::GetTreesStream>, Status> {
        self.reads
            .lock()
            .unwrap()
            .extend(q.get_ref().ids.iter().map(|id| (true, id.clone())));
        self.inner.get_trees(q).await
    }
    async fn get_blobs(
        &self,
        q: Request<p::ObjectRequest>,
    ) -> std::result::Result<Response<Self::GetBlobsStream>, Status> {
        self.reads
            .lock()
            .unwrap()
            .extend(q.get_ref().ids.iter().map(|id| (false, id.clone())));
        self.inner.get_blobs(q).await
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    meta: Arc<SqliteStore>,
    blobs: Arc<FsBlobStore>,
    backend: Arc<Backend>,
    id: ChangesetId,
    url: String,
    reads: ReadLog,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn metadata() -> p::CommitMetadata {
    let s = p::Signature {
        name: b"Author".to_vec(),
        email: b"a@b".to_vec(),
        timestamp: 1700000000,
        tz: b"+0000".to_vec(),
    };
    p::CommitMetadata {
        author: Some(s.clone()),
        committer: Some(s),
        message: b"remote".to_vec(),
    }
}
impl Fixture {
    async fn new() -> Self {
        Self::with_lost_land(false).await
    }
    async fn with_lost_land(lose: bool) -> Self {
        Self::configured(lose, false, false).await
    }
    async fn configured(lose: bool, strict: bool, retry: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_owned();
        let meta = Arc::new(SqliteStore::open(root.join("meta.db")).unwrap());
        let blobs = Arc::new(FsBlobStore::open(root.join("objects")).unwrap());
        let engine = Engine::standard();
        let ops = Operations {
            engine: &engine,
            ctx: Ctx {
                repo: Repository {
                    meta: meta.as_ref(),
                    blobs: blobs.as_ref(),
                },
            },
        };
        let file = |path: &[u8], raw: &[u8], executable| FileChange::Set {
            path: RepoPath::new(path).unwrap(),
            entry: Entry::File {
                blob: ops.ctx.repo.put_blob(raw).unwrap(),
                executable,
            },
        };
        let s = Signature {
            name: b"Author".as_slice().into(),
            email: b"a@b".as_slice().into(),
            timestamp: 1700000000,
            tz: b"+0000".as_slice().into(),
        };
        let id = ops
            .create_changeset(&Changeset {
                parents: vec![],
                author: s.clone(),
                committer: s,
                message: b"initial".as_slice().into(),
                git_provenance: None,
                changes: vec![
                    file(b"app/a", b"one\nbase\n", false),
                    FileChange::Set {
                        path: RepoPath::new(b"app/link".as_slice()).unwrap(),
                        entry: Entry::Symlink {
                            blob: ops.ctx.repo.put_blob(b"a").unwrap(),
                        },
                    },
                    file(b"app/sub/file", b"nested\n", false),
                    file(b"app/tool", b"#!/bin/sh\n", true),
                    file(b"raw/\xff", b"raw path\n", false),
                    file(b"secret/private", b"keep-secret\n", false),
                    FileChange::Set {
                        path: RepoPath::new(b"vendor/module".as_slice()).unwrap(),
                        entry: Entry::Gitlink {
                            commit: GitOid([0x11; 20]),
                        },
                    },
                ],
            })
            .unwrap();
        ops.ctx.repo.associate_changeset(b"repo", id).unwrap();
        meta.compare_and_swap(
            b"repo",
            &[RefUpdate {
                name: b"refs/heads/main".to_vec(),
                expected: RefState::default(),
                new: Some(id),
            }],
            b"fixture",
            1,
        )
        .unwrap();
        let backend = Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            blobs.clone(),
            meta.clone(),
            Some("token".into()),
        );
        let backend = backend
            .with_repository_accounts(vec![RepositoryAccount {
                name: "app-user".into(),
                token: "app-user-pat".into(),
                read_directories: Some(vec![RepoPath::new(b"app".as_slice()).unwrap()]),
                write_policy: Some(WritePolicy::default()),
            }])
            .unwrap();
        let backend = Arc::new(if strict {
            backend.with_object_token("server-hop".into()).unwrap()
        } else {
            backend
        });
        let reads = Arc::new(Mutex::new(vec![]));
        let observed = Observed {
            inner: ObjectService::new(backend.clone()),
            reads: reads.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let service = backend.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(MetadataService::new(service.clone()).into_server())
                .add_service(WriterServer::new(LostLand {
                    inner: WriteService::new(service),
                    lose: std::sync::atomic::AtomicBool::new(lose),
                    retry,
                }))
                .add_service(ObjectsServer::new(observed))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self {
            _temp: temp,
            root,
            meta,
            blobs,
            backend,
            id,
            url,
            reads,
            server,
        }
    }
    async fn cli(&self, path: &Path, args: &[&str]) -> Output {
        self.cli_as(path, args, "token").await
    }
    async fn cli_as(&self, path: &Path, args: &[&str], token: &str) -> Output {
        let token = token.to_owned();
        let path = path.to_owned();
        let args: Vec<_> = args.iter().map(|s| s.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_kasu"))
                .arg("-C")
                .arg(path)
                .args(args)
                .env("KASU_TOKEN", token)
                .env_remove("KASU_OBJECTS_ENDPOINT")
                .env("KASU_AUTHOR_NAME", "Client")
                .env("KASU_AUTHOR_EMAIL", "client@kasu.invalid")
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    }
    async fn ok(&self, path: &Path, args: &[&str]) -> String {
        let out = self.cli(path, args).await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    async fn checkout(&self, name: &str, sparse: &str) -> PathBuf {
        self.ok(
            &self.root,
            &[
                "checkout", "main", name, "--server", &self.url, "--repo", "repo", "--sparse",
                sparse,
            ],
        )
        .await;
        self.root.join(name)
    }
    fn remote_change(&self, path: &[u8], bytes: &[u8]) -> ChangesetId {
        let repo = Repository {
            meta: self.meta.as_ref(),
            blobs: self.blobs.as_ref(),
        };
        let blob = repo.put_blob(bytes).unwrap();
        repo.associate_blob(b"repo", blob).unwrap();
        let expected = self.meta.resolve(b"repo", b"refs/heads/main").unwrap();
        let cs = self
            .backend
            .create_changeset(p::CreateChangesetRequest {
                repo: b"repo".to_vec(),
                parents: vec![expected.target.unwrap().0.to_vec()],
                changes: vec![p::FileChange {
                    path: path.to_vec(),
                    entry: Some(p::Entry {
                        kind: Some(p::entry::Kind::File(p::File {
                            blob: blob.0.to_vec(),
                            executable: false,
                        })),
                    }),
                }],
                metadata: Some(metadata()),
            })
            .unwrap();
        let id = ChangesetId(cs.id.try_into().unwrap());
        assert!(matches!(
            self.meta
                .compare_and_swap(
                    b"repo",
                    &[RefUpdate {
                        name: b"refs/heads/main".to_vec(),
                        expected,
                        new: Some(id)
                    }],
                    b"other writer",
                    2
                )
                .unwrap(),
            CasResult::Applied(_)
        ));
        id
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_cli_sparse_checkout_amend_rebase_land_and_metadata_queries() {
    let f = Fixture::new().await;
    let wc = f.checkout("work", "app").await;
    assert_eq!(std::fs::read(wc.join("app/a")).unwrap(), b"one\nbase\n");
    assert!(!wc.join("secret").exists());
    assert_eq!(
        std::fs::read_link(wc.join("app/link")).unwrap(),
        Path::new("a")
    );
    assert_ne!(
        std::fs::metadata(wc.join("app/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(f.ok(&wc, &["status"]).await, "");
    // Exact wire requests, including the symlink blob; the hidden/raw/vendor
    // subtrees never enter the client's object cache during sparse checkout.
    let repo = Repository {
        meta: f.meta.as_ref(),
        blobs: f.blobs.as_ref(),
    };
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx { repo },
    };
    let root = ops.root_tree(f.id).unwrap();
    let TreeEntry::SubTree(app) = ops.ctx.repo.tree(root).unwrap().entries[b"app".as_slice()]
    else {
        panic!()
    };
    let tree = ops.ctx.repo.tree(app).unwrap();
    let mut expected = BTreeSet::from([(true, root.0.to_vec()), (true, app.0.to_vec())]);
    for value in tree.entries.values() {
        match value {
            TreeEntry::Leaf(entry) => {
                expected.insert((false, entry.blob().unwrap().0.to_vec()));
            }
            TreeEntry::SubTree(id) => {
                expected.insert((true, id.0.to_vec()));
                for value in ops.ctx.repo.tree(*id).unwrap().entries.values() {
                    let TreeEntry::Leaf(entry) = value else {
                        panic!()
                    };
                    expected.insert((false, entry.blob().unwrap().0.to_vec()));
                }
            }
        }
    }
    assert_eq!(
        f.reads
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        expected
    );
    let before = f.reads.lock().unwrap().len();
    f.ok(&wc, &["sparse", "remove", "app/sub"]).await;
    assert!(!wc.join("app/sub").exists());
    assert_eq!(f.reads.lock().unwrap().len(), before);
    std::fs::write(wc.join("app/a"), b"local\nbase\n").unwrap();
    std::fs::write(wc.join("app/new"), b"first\n").unwrap();
    assert!(f.ok(&wc, &["diff"]).await.contains("+local"));
    f.ok(&wc, &["commit", "-m", "first draft"]).await;
    std::fs::write(wc.join("app/new"), b"second\n").unwrap();
    std::fs::write(wc.join("app/second"), b"also kept\n").unwrap();
    let draft = f
        .ok(&wc, &["commit", "-m", "amended draft"])
        .await
        .trim()
        .parse::<ChangesetId>()
        .unwrap();
    assert_eq!(ops.ctx.repo.changeset(draft).unwrap().parents, vec![f.id]);
    let remote = f.remote_change(b"secret/private", b"remote change\n");
    f.ok(&wc, &["rebase", "--onto", "main"]).await;
    f.ok(&wc, &["land"]).await;
    assert_eq!(std::fs::read(wc.join("app/a")).unwrap(), b"local\nbase\n");
    assert_eq!(std::fs::read(wc.join("app/new")).unwrap(), b"second\n");
    assert_eq!(f.ok(&wc, &["status"]).await, "");
    let head = f
        .meta
        .resolve(b"repo", b"refs/heads/main")
        .unwrap()
        .target
        .unwrap();
    assert!(ops.is_ancestor(remote, head).unwrap());
    assert_eq!(
        f.ok(&wc, &["show", "main", "secret/private"]).await,
        "remote change\n"
    );
    assert!(
        f.ok(&wc, &["log", "--limit", "3"])
            .await
            .contains("amended draft")
    );
    f.ok(&wc, &["blame", "app/new"]).await;
    f.ok(&wc, &["blame", "--follow-copies", "app/new"]).await;
    f.ok(&wc, &["blame", "--follow-moves", "app/new"]).await;
    f.ok(&wc, &["sparse", "add", "secret"]).await;
    assert_eq!(
        std::fs::read(wc.join("secret/private")).unwrap(),
        b"remote change\n"
    );
    std::fs::write(wc.join("secret/private"), b"dirty\n").unwrap();
    assert!(
        !f.cli(&wc, &["sparse", "remove", "secret"])
            .await
            .status
            .success()
    );
    assert_eq!(
        std::fs::read(wc.join("secret/private")).unwrap(),
        b"dirty\n"
    );
    assert!(
        !std::fs::read_to_string(wc.join(".kasu/state.json"))
            .unwrap()
            .contains("token")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicts_replay_without_losing_files_and_gitlinks_can_be_committed() {
    let f = Fixture::new().await;
    let a = f.checkout("a", "app").await;
    let b = f.checkout("b", "app").await;
    std::fs::write(a.join("app/a"), b"A\nbase\n").unwrap();
    std::fs::write(b.join("app/a"), b"B\nbase\n").unwrap();
    f.ok(&a, &["commit", "-m", "A"]).await;
    f.ok(&b, &["commit", "-m", "B"]).await;
    f.ok(&a, &["land"]).await;
    assert!(!f.cli(&b, &["land"]).await.status.success());
    assert_eq!(std::fs::read(b.join("app/a")).unwrap(), b"B\nbase\n");
    assert!(b.join(".kasu/conflicts.json").is_file());
    let state: kasu_cli::State =
        serde_json::from_slice(&std::fs::read(b.join(".kasu/state.json")).unwrap()).unwrap();
    let sequence = f.meta.tail(b"repo", 0, 100).unwrap().len();
    let replay = f.cli(&b, &["--json", "land"]).await;
    assert!(!replay.status.success());
    let response: p::LandResponse = serde_json::from_slice(&replay.stdout).unwrap();
    assert!(response.replayed);
    assert_eq!(sequence, f.meta.tail(b"repo", 0, 100).unwrap().len());
    let next: kasu_cli::State =
        serde_json::from_slice(&std::fs::read(b.join(".kasu/state.json")).unwrap()).unwrap();
    assert_eq!(next.land_key, state.land_key);
    std::fs::write(b.join("app/a"), b"A\nbase\n").unwrap();
    f.ok(&b, &["commit", "-m", "resolve"]).await;
    f.ok(&b, &["rebase", "--onto", "main"]).await;
    f.ok(&b, &["land"]).await;
    let vendor = f.checkout("vendor-work", "vendor").await;
    let marker = vendor.join("vendor/module/.kasu-gitlink");
    assert!(marker.is_file());
    assert_eq!(f.ok(&vendor, &["status"]).await, "");
    std::fs::write(marker, format!("{}\n", GitOid([0x22; 20]))).unwrap();
    f.ok(&vendor, &["commit", "-m", "gitlink"]).await;
    f.ok(&vendor, &["land"]).await;
    assert_eq!(f.ok(&vendor, &["status"]).await, "");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untracked_files_existing_destinations_and_symlink_parents_are_preserved() {
    let f = Fixture::new().await;
    let wc = f.checkout("work", "app").await;
    std::fs::create_dir(wc.join("secret")).unwrap();
    std::fs::write(wc.join("secret/private"), b"untracked").unwrap();
    assert!(
        !f.cli(&wc, &["sparse", "add", "secret"])
            .await
            .status
            .success()
    );
    assert_eq!(
        std::fs::read(wc.join("secret/private")).unwrap(),
        b"untracked"
    );
    assert!(
        !f.cli(
            &f.root,
            &[
                "checkout", "main", "work", "--server", &f.url, "--repo", "repo", "--sparse", "app"
            ]
        )
        .await
        .status
        .success()
    );
    let outside = f.root.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("a"), b"outside").unwrap();
    std::fs::rename(wc.join("app"), f.root.join("saved-app")).unwrap();
    symlink(&outside, wc.join("app")).unwrap();
    assert!(!f.cli(&wc, &["rebase"]).await.status.success());
    assert_eq!(std::fs::read(outside.join("a")).unwrap(), b"outside");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_watch_observes_real_edits_and_does_not_hold_the_working_copy_lock() {
    use std::io::BufRead;
    let f = Fixture::new().await;
    let wc = f.checkout("watch", "app").await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_kasu"))
        .arg("-C")
        .arg(&wc)
        .args(["--json", "status", "--watch"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "[]"
    );
    assert_eq!(f.ok(&wc, &["status"]).await, "");
    std::fs::write(wc.join("app/a"), b"watched\nbase\n").unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(line) = rx.recv().await {
            if line.contains("after") {
                return true;
            }
        }
        false
    })
    .await;
    let _ = child.kill();
    child.wait().unwrap();
    reader.join().unwrap();
    assert!(event.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn amending_an_addition_to_deletion_and_file_directory_type_changes() {
    let f = Fixture::new().await;
    let wc = f.checkout("types", "app").await;
    std::fs::write(wc.join("app/added"), b"temporary").unwrap();
    f.ok(&wc, &["commit", "-m", "add"]).await;
    std::fs::remove_file(wc.join("app/added")).unwrap();
    f.ok(&wc, &["commit", "-m", "cancel addition"]).await;
    std::fs::remove_file(wc.join("app/a")).unwrap();
    std::fs::create_dir(wc.join("app/a")).unwrap();
    std::fs::write(wc.join("app/a/child"), b"child").unwrap();
    f.ok(&wc, &["commit", "-m", "file to directory"]).await;
    f.ok(&wc, &["land"]).await;
    std::fs::remove_file(wc.join("app/a/child")).unwrap();
    std::fs::remove_dir(wc.join("app/a")).unwrap();
    std::fs::write(wc.join("app/a"), b"file again").unwrap();
    f.ok(&wc, &["commit", "-m", "directory to file"]).await;
    f.ok(&wc, &["land"]).await;
    assert_eq!(f.ok(&wc, &["status"]).await, "");
    assert_eq!(f.ok(&wc, &["show", "main", "app/a"]).await, "file again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn excluded_new_subtrees_stay_unrequested_and_specific_includes_override_exclusions() {
    let f = Fixture::new().await;
    let wc = f.checkout("profile", "app").await;
    f.ok(&wc, &["sparse", "remove", "app/sub"]).await;
    let id = f.remote_change(b"app/sub/new", b"new invisible\n");
    f.reads.lock().unwrap().clear();
    f.ok(&wc, &["rebase"]).await;
    let repo = Repository {
        meta: f.meta.as_ref(),
        blobs: f.blobs.as_ref(),
    };
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx { repo },
    };
    let root = ops.root_tree(id).unwrap();
    let TreeEntry::SubTree(app) = ops.ctx.repo.tree(root).unwrap().entries[b"app".as_slice()]
    else {
        panic!()
    };
    let TreeEntry::SubTree(sub) = ops.ctx.repo.tree(app).unwrap().entries[b"sub".as_slice()] else {
        panic!()
    };
    let new = BlobId::hash(b"new invisible\n");
    assert!(!f.reads.lock().unwrap().contains(&(true, sub.0.to_vec())));
    assert!(!f.reads.lock().unwrap().contains(&(false, new.0.to_vec())));
    f.ok(&wc, &["sparse", "add", "app/sub/file"]).await;
    assert!(wc.join("app/sub/file").is_file());
    assert!(!wc.join("app/sub/new").exists());
    assert!(!f.reads.lock().unwrap().contains(&(false, new.0.to_vec())));
    f.ok(&wc, &["sparse", "add", "app/sub"]).await;
    assert_eq!(
        std::fs::read(wc.join("app/sub/new")).unwrap(),
        b"new invisible\n"
    );
    assert!(f.reads.lock().unwrap().contains(&(false, new.0.to_vec())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn byte_paths_survive_the_remote_snapshot_and_platform_checkout_rules() {
    use std::os::unix::ffi::OsStrExt;
    let f = Fixture::new().await;
    let cache = FsBlobStore::open(f.root.join("raw-cache")).unwrap();
    let mut remote = kasu_cli::Remote::connect(&f.url, &f.url, b"repo".to_vec(), Some("token"))
        .await
        .unwrap();
    let snapshot = remote
        .snapshot(
            &cache,
            f.id,
            &kasu_cli::SparseProfile::new(vec![b"raw".to_vec()]).unwrap(),
        )
        .await
        .unwrap();
    assert!(snapshot.contains_key(b"raw/\xff".as_slice()));
    let output = f
        .cli(
            &f.root,
            &[
                "checkout", "main", "raw-work", "--server", &f.url, "--repo", "repo", "--sparse",
                "raw",
            ],
        )
        .await;
    #[cfg(target_os = "linux")]
    {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(
                f.root
                    .join("raw-work")
                    .join(std::ffi::OsStr::from_bytes(b"raw/\xff"))
            )
            .unwrap(),
            b"raw path\n"
        );
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::ffi::OsStr::from_bytes(b"raw/\xff");
        assert!(!output.status.success());
        assert!(!f.root.join("raw-work").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_land_reply_and_retry_retain_the_key_and_block_rewrites_until_replay() {
    for retry in [false, true] {
        let f = Fixture::configured(true, false, retry).await;
        let wc = f.checkout("lost", "app").await;
        std::fs::write(wc.join("app/a"), b"landed once\n").unwrap();
        f.ok(&wc, &["commit", "-m", "lost reply"]).await;
        assert!(!f.cli(&wc, &["land"]).await.status.success());
        let state: kasu_cli::State =
            serde_json::from_slice(&std::fs::read(wc.join(".kasu/state.json")).unwrap()).unwrap();
        assert!(state.land_key.is_some());
        assert_eq!(
            state.land_outcome,
            retry.then_some(p::LandOutcome::Retry as i32)
        );
        let head = f.meta.resolve(b"repo", b"refs/heads/main").unwrap();
        let sequence = f.meta.tail(b"repo", 0, 100).unwrap().len();
        std::fs::write(wc.join("app/a"), b"later edit\n").unwrap();
        let output = f.cli(&wc, &["commit", "-m", "must wait"]).await;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("pending Land must be replayed"));
        assert_eq!(std::fs::read(wc.join("app/a")).unwrap(), b"later edit\n");
        std::fs::write(wc.join("app/a"), b"landed once\n").unwrap();
        let output = f.cli(&wc, &["rebase", "--onto", "main"]).await;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("pending Land must be replayed"));
        let pending = kasu_cli::WorkingCopy::open(&wc).unwrap();
        assert_eq!(pending.state.land_key, state.land_key);
        assert_eq!(pending.state.base, state.base);
        drop(pending);
        let response: p::LandResponse =
            serde_json::from_str(&f.ok(&wc, &["--json", "land"]).await).unwrap();
        assert!(response.replayed);
        assert_eq!(response.id, head.target.map(|id| id.0.to_vec()));
        assert_eq!(sequence, f.meta.tail(b"repo", 0, 100).unwrap().len());
        assert_eq!(f.ok(&wc, &["status"]).await, "");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebase_by_changeset_id_preserves_the_land_branch() {
    let f = Fixture::new().await;
    f.meta
        .compare_and_swap(
            b"repo",
            &[RefUpdate {
                name: b"refs/heads/feature".to_vec(),
                expected: RefState::default(),
                new: Some(f.id),
            }],
            b"fixture",
            0,
        )
        .unwrap();
    f.ok(
        &f.root,
        &[
            "checkout",
            "feature",
            "feature-work",
            "--server",
            &f.url,
            "--repo",
            "repo",
            "--sparse",
            "app",
        ],
    )
    .await;
    let wc = f.root.join("feature-work");
    for draft in [false, true] {
        if draft {
            std::fs::write(wc.join("app/a"), b"feature work\n").unwrap();
            f.ok(&wc, &["commit", "-m", "feature change"]).await;
        }
        f.ok(&wc, &["rebase", "--onto", &f.id.to_string()]).await;
        let copy = kasu_cli::WorkingCopy::open(&wc).unwrap();
        assert_eq!(copy.state.reference, b"refs/heads/feature");
    }
    f.ok(&wc, &["land"]).await;
    assert_eq!(
        f.meta.resolve(b"repo", b"refs/heads/main").unwrap().target,
        Some(f.id)
    );
    assert_ne!(
        f.meta
            .resolve(b"repo", b"refs/heads/feature")
            .unwrap()
            .target,
        Some(f.id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checkout_by_changeset_id_requires_an_explicit_land_branch() {
    let f = Fixture::new().await;
    f.ok(
        &f.root,
        &[
            "checkout",
            &f.id.to_string(),
            "detached",
            "--server",
            &f.url,
            "--repo",
            "repo",
            "--sparse",
            "app",
        ],
    )
    .await;
    let wc = f.root.join("detached");
    std::fs::write(wc.join("app/a"), b"detached work\n").unwrap();
    f.ok(&wc, &["commit", "-m", "detached change"]).await;
    f.ok(&wc, &["rebase", "--onto", &f.id.to_string()]).await;
    let output = f.cli(&wc, &["land"]).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("specify --ref"));
    let copy = kasu_cli::WorkingCopy::open(&wc).unwrap();
    assert!(copy.state.reference.is_empty());
    assert!(copy.state.land_key.is_none());
    let draft = copy.state.id().unwrap();
    drop(copy);
    f.ok(&wc, &["land", "--ref", "feature"]).await;
    assert_eq!(
        f.meta.resolve(b"repo", b"refs/heads/main").unwrap().target,
        Some(f.id)
    );
    assert_eq!(
        f.meta
            .resolve(b"repo", b"refs/heads/feature")
            .unwrap()
            .target,
        Some(draft)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lazy_blob_reads_verify_content_hashes() {
    let f = Fixture::new().await;
    f.backend
        .get_root_tree(p::ChangesetRequest {
            repo: b"repo".to_vec(),
            id: f.id.0.to_vec(),
        })
        .unwrap();
    let mut remote = kasu_cli::Remote::connect(&f.url, &f.url, b"repo".to_vec(), Some("token"))
        .await
        .unwrap();
    let id = BlobId::hash(b"one\nbase\n");
    assert_eq!(remote.fetch_blob_bytes(id).await.unwrap(), b"one\nbase\n");
    let hex = id.to_string();
    std::fs::write(
        f.root.join("objects").join(&hex[..2]).join(&hex[2..]),
        compress(b"bad\nblob\n").unwrap(),
    )
    .unwrap();
    let error = remote.fetch_blob_bytes(id).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<Status>().unwrap().code(),
        tonic::Code::DataLoss
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn real_cli_uses_the_unix_daemon_and_a_snapshot_can_boot_without_the_cache() {
    use kasu_cache::{DiskStore, Layer, Limits, MemoryStore, Service};
    use kasu_object::{Client, Layer as Role};
    struct Task(tokio::task::JoinHandle<()>);
    impl Drop for Task {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let f = Fixture::configured(false, true, false).await;
    let direct = f
        .cli(
            &f.root,
            &[
                "checkout", "main", "direct", "--server", &f.url, "--repo", "repo", "--sparse",
                "app",
            ],
        )
        .await;
    assert!(!direct.status.success());
    assert!(!f.root.join("direct").exists());
    let limits = Limits {
        qps: 10_000,
        ..Default::default()
    };
    let cache = Layer::new(
        Role::Cache,
        b"repo".to_vec(),
        "daemon-hop",
        "server-hop",
        Client::connect(&f.url, Some("server-hop"), Some(Role::Server))
            .await
            .unwrap(),
        Arc::new(MemoryStore::new(1024 * 1024)),
        limits.clone(),
    )
    .unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", tcp.local_addr().unwrap());
    let layer = cache.clone();
    let _cache = Task(tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(Service::new(layer).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(tcp))
            .await
            .unwrap();
    }));
    let disk = Arc::new(DiskStore::open(&f.root.join("daemon-data"), b"repo").unwrap());
    let daemon = Layer::new(
        Role::Daemon,
        b"repo".to_vec(),
        "token",
        "daemon-hop",
        Client::connect(&url, Some("daemon-hop"), Some(Role::Cache))
            .await
            .unwrap(),
        disk.clone(),
        limits.clone(),
    )
    .unwrap();
    let runtime = f.root.join("run");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = runtime.join("node.sock");
    let (uds, _lease) = kasu_daemon::SocketLease::bind(&socket).unwrap();
    let layer = daemon.clone();
    let _node = Task(tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(Service::new(layer).into_server())
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
            .await
            .unwrap();
    }));
    let object_url = format!("unix://{}", socket.display());
    f.ok(
        &f.root,
        &[
            "checkout",
            "main",
            "layered",
            "--server",
            &f.url,
            "--objects",
            &object_url,
            "--repo",
            "repo",
            "--sparse",
            "app",
        ],
    )
    .await;
    assert_eq!(
        std::fs::read(f.root.join("layered/app/a")).unwrap(),
        b"one\nbase\n"
    );
    assert_eq!(daemon.stats().entries, 7);
    assert_eq!(cache.stats().upstream, 7);
    let snapshot = f.root.join("node-snapshot.tar");
    assert_eq!(disk.export_snapshot(&snapshot).unwrap(), 7);
    let restored = Arc::new(DiskStore::open(&f.root.join("restored-data"), b"repo").unwrap());
    restored.import_snapshot(&snapshot, 1024 * 1024).unwrap();
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let upper = Client::new(
        kasu_object::lazy(&unavailable).unwrap(),
        Some("daemon-hop"),
        Some(Role::Cache),
    )
    .unwrap();
    let node2 = Layer::new(
        Role::Daemon,
        b"repo".to_vec(),
        "token",
        "daemon-hop",
        upper,
        restored,
        limits,
    )
    .unwrap();
    let socket2 = runtime.join("restored.sock");
    let (uds, _lease2) = kasu_daemon::SocketLease::bind(&socket2).unwrap();
    let layer = node2.clone();
    let _node2 = Task(tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(Service::new(layer).into_server())
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(uds))
            .await
            .unwrap();
    }));
    let before = f.reads.lock().unwrap().len();
    let objects2 = format!("unix://{}", socket2.display());
    f.ok(
        &f.root,
        &[
            "checkout",
            "main",
            "offline-warm",
            "--server",
            &f.url,
            "--objects",
            &objects2,
            "--repo",
            "repo",
            "--sparse",
            "app",
        ],
    )
    .await;
    assert_eq!(node2.stats().upstream, 0);
    assert_eq!(node2.stats().hits, 7);
    assert_eq!(f.reads.lock().unwrap().len(), before);
    assert!(
        !f.cli(&f.root.join("offline-warm"), &["sparse", "add", "secret"])
            .await
            .status
            .success()
    );
    assert_eq!(f.reads.lock().unwrap().len(), before);
    assert!(!f.root.join("offline-warm/secret").exists());
    let working = f.root.join("layered");
    let state_path = working.join(".kasu/state.json");
    let mut state: kasu_cli::State =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    state.objects = f.url.clone();
    std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(
        !f.cli(&working, &["sparse", "add", "secret"])
            .await
            .status
            .success()
    );
    f.ok(
        &working,
        &["--objects", &object_url, "sparse", "add", "secret"],
    )
    .await;
    assert!(working.join("secret/private").is_file());
    let state: kasu_cli::State =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(state.objects, f.url);
    let work = working.clone();
    let endpoint = object_url.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_kasu"))
            .arg("-C")
            .arg(work)
            .args(["sparse", "add", "vendor"])
            .env("KASU_TOKEN", "token")
            .env("KASU_OBJECTS_ENDPOINT", endpoint)
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
    assert!(working.join("vendor/module/.kasu-gitlink").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_cli_blame_flags_change_moved_and_copied_line_origins() {
    let f = Fixture::new().await;
    let alpha = b"alpha original function with enough letters for tracking\nalpha body returns an original alpha value\n";
    let beta = b"beta independent function with enough letters for tracking\nbeta body returns an original beta value\n";
    let original = f.remote_change(b"app/source", &[alpha.as_slice(), beta.as_slice()].concat());
    let moved = f.remote_change(b"app/source", &[beta.as_slice(), alpha.as_slice()].concat());
    let copied = f.remote_change(
        b"app/dest",
        &[b"new destination header\n".as_slice(), alpha.as_slice()].concat(),
    );
    let wc = f.checkout("blame-flags", "app").await;
    let plain: p::BlameResponse =
        serde_json::from_str(&f.ok(&wc, &["blame", "app/source"]).await).unwrap();
    assert!(plain.lines.iter().any(|line| line.changeset == moved.0));
    let follows: p::BlameResponse =
        serde_json::from_str(&f.ok(&wc, &["blame", "--follow-moves", "app/source"]).await).unwrap();
    assert!(
        follows
            .lines
            .iter()
            .all(|line| line.changeset == original.0 && line.path == b"app/source")
    );
    assert_eq!(
        follows.lines.iter().map(|l| l.line).collect::<Vec<_>>(),
        vec![3, 4, 1, 2]
    );
    let moves_only: p::BlameResponse =
        serde_json::from_str(&f.ok(&wc, &["blame", "--follow-moves", "app/dest"]).await).unwrap();
    assert!(
        moves_only
            .lines
            .iter()
            .all(|line| line.changeset == copied.0)
    );
    let copies: p::BlameResponse =
        serde_json::from_str(&f.ok(&wc, &["blame", "--follow-copies", "app/dest"]).await).unwrap();
    assert_eq!(copies.lines[0].changeset, copied.0);
    for (i, line) in copies.lines[1..].iter().enumerate() {
        assert_eq!(line.changeset, original.0);
        assert_eq!(line.path, b"app/source");
        assert_eq!(line.line, i as u64 + 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_snapshot_and_blob_sizes_do_not_download_file_contents() {
    let f = Fixture::new().await;
    let mut remote = kasu_cli::Remote::connect(&f.url, &f.url, b"repo".to_vec(), Some("token"))
        .await
        .unwrap();
    let cache = FsBlobStore::open(f.root.join("lazy-cache")).unwrap();
    let snapshot = remote
        .metadata_snapshot(
            &cache,
            f.id,
            &kasu_cli::SparseProfile::new(vec![b"app".to_vec()]).unwrap(),
        )
        .await
        .unwrap();
    let ids: BTreeSet<_> = snapshot.values().filter_map(Entry::blob).collect();
    let sizes = remote
        .blob_sizes(&ids.iter().copied().collect::<Vec<_>>())
        .await
        .unwrap();
    assert_eq!(snapshot.len(), 4);
    assert_eq!(sizes.len(), 4);
    for id in ids {
        let metadata = Repository {
            meta: f.meta.as_ref(),
            blobs: f.blobs.as_ref(),
        }
        .blob_metadata(id)
        .unwrap();
        assert_eq!(sizes[&id], metadata.size);
        assert!(matches!(cache.get(ObjectId(id.0)), Err(Error::NotFound(_))));
    }
    assert!(f.reads.lock().unwrap().iter().all(|(tree, _)| *tree));
    assert_eq!(
        f.backend
            .get_blob_metadata(p::ObjectRequest {
                repo: b"repo".to_vec(),
                ids: vec![]
            })
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires /dev/fuse and CAP_SYS_ADMIN; tools/virtual/Dockerfile"]
async fn virtual_mount_reads_lazily_and_commits_through_copy_on_write_and_remount() {
    use std::process::{Child, Stdio};
    struct Mount(Child);
    impl Drop for Mount {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let f = Fixture::new().await;
    let wc = f.root.join("virtual");
    let backing = f.root.join("backing");
    let log = f.root.join("mount.log");
    let start = || {
        let stdout = std::fs::File::create(&log).unwrap();
        Mount(
            Command::new(env!("CARGO_BIN_EXE_kasu"))
                .arg("-C")
                .arg(&f.root)
                .args([
                    "mount",
                    "main",
                    "virtual",
                    "--backing",
                    "backing",
                    "--server",
                    &f.url,
                    "--repo",
                    "repo",
                    "--sparse",
                    "app",
                    "--sparse",
                    "raw",
                    "--sparse",
                    "vendor",
                ])
                .env("KASU_TOKEN", "token")
                .env_remove("KASU_OBJECTS_ENDPOINT")
                .stdout(stdout.try_clone().unwrap())
                .stderr(Stdio::from(stdout))
                .spawn()
                .unwrap(),
        )
    };
    let ready = |mount: &mut Mount| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("mounted ") {
                return;
            }
            assert!(
                mount.0.try_wait().unwrap().is_none(),
                "mount exited: {text}"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!(
            "mount did not become ready: {}",
            std::fs::read_to_string(&log).unwrap()
        );
    };
    let mut mount = start();
    ready(&mut mount);
    assert_eq!(std::fs::metadata(wc.join("app/a")).unwrap().len(), 9);
    assert!(std::fs::read_dir(&wc).unwrap().count() > 0);
    assert!(!wc.join("secret").exists());
    assert!(
        f.reads.lock().unwrap().iter().all(|(tree, _)| *tree),
        "stat/readdir must not download blobs"
    );
    assert_eq!(std::fs::read(wc.join("app/a")).unwrap(), b"one\nbase\n");
    let reads = f
        .reads
        .lock()
        .unwrap()
        .iter()
        .filter(|(tree, _)| !*tree)
        .count();
    assert_eq!(reads, 1);
    assert_eq!(std::fs::read(wc.join("app/a")).unwrap(), b"one\nbase\n");
    assert_eq!(
        f.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(tree, _)| !*tree)
            .count(),
        reads
    );
    assert!(
        !backing.join("upper/app/a").exists(),
        "reads do not materialize working files"
    );
    use std::os::unix::ffi::OsStringExt;
    assert_eq!(
        std::fs::read(wc.join(PathBuf::from(std::ffi::OsString::from_vec(
            b"raw/\xff".to_vec()
        ))))
        .unwrap(),
        b"raw path\n"
    );
    assert_eq!(
        std::fs::read_to_string(wc.join("vendor/module/.kasu-gitlink")).unwrap(),
        format!("{}\n", GitOid([0x11; 20]))
    );
    assert_ne!(
        std::fs::metadata(wc.join("app/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        std::fs::read_link(wc.join("app/link")).unwrap(),
        Path::new("a")
    );
    std::fs::write(wc.join("app/a"), b"virtual edit\n").unwrap();
    std::fs::write(wc.join("app/new"), b"new virtual file\n").unwrap();
    std::fs::remove_file(wc.join("app/tool")).unwrap();
    assert!(backing.join("upper/app/a").is_file());
    assert!(!backing.join("upper/app/sub/file").exists());
    assert!(f.ok(&wc, &["status"]).await.contains("app/a"));
    assert!(f.ok(&wc, &["diff"]).await.contains("virtual edit"));
    f.ok(&wc, &["commit", "-m", "virtual commit"]).await;
    f.remote_change(b"app/remote", b"concurrent upstream change\n");
    f.ok(&wc, &["rebase", "--onto", "main"]).await;
    assert_eq!(
        std::fs::read(wc.join("app/remote")).unwrap(),
        b"concurrent upstream change\n"
    );
    f.ok(&wc, &["land"]).await;
    assert!(f.ok(&wc, &["status"]).await.is_empty());
    assert_eq!(
        f.ok(&wc, &["show", "main", "app/a"]).await,
        "virtual edit\n"
    );
    assert!(
        !backing.join("upper/app/sub/file").exists(),
        "native status/commit must not copy unchanged working files up"
    );
    // SIGKILL leaves the mounts behind. The persisted origin and upper files
    // must survive recovery after the owner lock has been released.
    mount.0.kill().unwrap();
    mount.0.wait().unwrap();
    let mut mount = start();
    ready(&mut mount);
    assert_eq!(std::fs::read(wc.join("app/a")).unwrap(), b"virtual edit\n");
    assert_eq!(
        std::fs::read(wc.join("app/new")).unwrap(),
        b"new virtual file\n"
    );
    assert!(!wc.join("app/tool").exists());
    assert!(f.ok(&wc, &["status"]).await.is_empty());
    let held = std::fs::File::open(wc.join("app/sub/file")).unwrap();
    assert!(
        Command::new("kill")
            .args(["-TERM", &mount.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&log)
            .unwrap()
            .contains("close open files")
        {
            break;
        }
        assert!(
            mount.0.try_wait().unwrap().is_none(),
            "busy mount owner exited"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "busy unmount was not reported"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read(wc.join("app/sub/file")).unwrap(), b"nested\n");
    drop(held);
    assert!(
        Command::new("kill")
            .args(["-TERM", &mount.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = mount.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "{}",
                std::fs::read_to_string(&log).unwrap()
            );
            break;
        }
        assert!(std::time::Instant::now() < deadline, "unmount timed out");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(std::fs::read_dir(&wc).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_scoped_account_checks_out_commits_and_lands_without_hidden_objects() {
    let f = Fixture::configured(false, true, false).await;
    let check = |o: Output| {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        o
    };
    check(
        f.cli_as(
            &f.root,
            &[
                "checkout", "main", "scoped", "--server", &f.url, "--repo", "repo",
            ],
            "app-user-pat",
        )
        .await,
    );
    let wc = f.root.join("scoped");
    assert_eq!(std::fs::read(wc.join("app/a")).unwrap(), b"one\nbase\n");
    assert!(!wc.join("secret").exists());
    assert!(!wc.join("raw").exists());
    assert!(!wc.join("vendor").exists());
    let reads = f.reads.lock().unwrap().clone();
    let hidden = ObjectId::hash(b"keep-secret\n").0.to_vec();
    assert!(!reads.iter().any(|(_, id)| *id == hidden));
    check(f.cli_as(&wc, &["status"], "app-user-pat").await);
    let denied = f
        .cli_as(&wc, &["show", "main", "secret/private"], "app-user-pat")
        .await;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("outside readable directories"));
    std::fs::write(wc.join("app/a"), b"scoped update\n").unwrap();
    check(
        f.cli_as(&wc, &["commit", "-m", "scoped draft"], "app-user-pat")
            .await,
    );
    check(f.cli_as(&wc, &["land"], "app-user-pat").await);
    let id = f
        .meta
        .resolve(b"repo", b"refs/heads/main")
        .unwrap()
        .target
        .unwrap();
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: f.meta.as_ref(),
                blobs: f.blobs.as_ref(),
            },
        },
    };
    assert_eq!(
        ops.read_file(id, &RepoPath::new(b"secret/private".as_slice()).unwrap())
            .unwrap()
            .1,
        b"keep-secret\n"
    );
    assert_eq!(
        ops.read_file(id, &RepoPath::new(b"app/a".as_slice()).unwrap())
            .unwrap()
            .1,
        b"scoped update\n"
    );
    std::fs::create_dir(wc.join("secret")).unwrap();
    std::fs::write(wc.join("secret/new"), b"denied").unwrap();
    assert!(
        !f.cli_as(&wc, &["commit", "-m", "outside scope"], "app-user-pat")
            .await
            .status
            .success()
    );
    assert_eq!(
        f.meta.resolve(b"repo", b"refs/heads/main").unwrap().target,
        Some(id)
    );
}
