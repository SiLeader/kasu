use kasu_core::*;
use kasu_derived::Engine;
use kasu_git::*;
use kasu_ingest::Importer;
use kasu_server::*;
use kasu_store::*;
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::Arc,
};
fn git(path: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git at {path:?} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_multi_ref_fetch_preserves_divergent_and_merged_histories() {
    let running = Running::new(false).await;
    let mut clones = Vec::new();
    for version in 0..=2 {
        for shallow in [false, true] {
            let path = running
                ._dir
                .path()
                .join(format!("series-{version}-{shallow}.git"));
            let mut args = vec!["clone".into(), "--bare".into()];
            if shallow {
                args.push("--depth=1".into());
            }
            args.extend([running.url.clone(), path.to_str().unwrap().into()]);
            success(client(running._dir.path(), version, args, None).await);
            clones.push((version, shallow, path));
        }
    }
    let mut main = running.tip;
    let mut release =
        oid_from_hex(git(&running.source, &["rev-parse", "refs/heads/release"], b"").trim_ascii())
            .unwrap();
    for round in 0..8 {
        release = commit(&running.source, 100 + round * 2, &[release]);
        let parents = if round % 2 == 0 {
            vec![main]
        } else {
            vec![main, release]
        };
        main = commit(&running.source, 101 + round * 2, &parents);
        let tag = object(&running.source, "tag", format!(
            "object {release}\ntype commit\ntag round-{round}\ntagger A <a@b> 1700000200 +0000\n\nrelease {round}\n"
        ).as_bytes());
        for (name, oid) in [
            ("refs/heads/main".into(), main),
            ("refs/heads/release".into(), release),
            (format!("refs/tags/round-{round}"), tag),
        ] {
            git(
                &running.source,
                &["update-ref", &name, &oid.to_string()],
                b"",
            );
        }
        let mut engine = Engine::standard();
        register_git(&mut engine);
        let importer = Importer {
            engine: &engine,
            index: GitIndex {
                repo: Repository {
                    meta: running.meta.as_ref(),
                    blobs: running.blobs.as_ref(),
                },
                projection: ProjectionId::full(b"repo".as_slice()),
            },
        };
        let report = importer.import(&running.source).unwrap();
        importer
            .publish(running.meta.as_ref(), &report, b"series", round.into())
            .unwrap();
        let expected: BTreeSet<_> = git(
            &running.source,
            &["rev-list", "--objects", "--all", "--no-object-names"],
            b"",
        )
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .map(Vec::from)
        .collect();
        for (version, shallow, path) in &clones {
            success(
                client(
                    path,
                    *version,
                    vec![
                        "fetch".into(),
                        "origin".into(),
                        "refs/heads/*:refs/heads/*".into(),
                        "refs/tags/*:refs/tags/*".into(),
                    ],
                    None,
                )
                .await,
            );
            git(path, &["fsck", "--strict"], b"");
            for (name, oid) in [("refs/heads/main", main), ("refs/heads/release", release)] {
                assert_eq!(
                    git(path, &["rev-parse", name], b"").trim_ascii(),
                    oid.to_string().as_bytes(),
                    "round={round} v{version} shallow={shallow}"
                );
            }
            if !shallow {
                let actual: BTreeSet<_> = git(
                    path,
                    &["rev-list", "--objects", "--all", "--no-object-names"],
                    b"",
                )
                .split(|b| *b == b'\n')
                .filter(|s| !s.is_empty())
                .map(Vec::from)
                .collect();
                assert_eq!(actual, expected, "round={round} v{version}");
            }
        }
    }
}
fn object(path: &Path, kind: &str, raw: &[u8]) -> GitOid {
    oid_from_hex(git(path, &["hash-object", "-w", "-t", kind, "--stdin"], raw).trim_ascii())
        .unwrap()
}

#[tokio::test]
async fn path_projection_url_suffix_preserves_dot_git_directory_names() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use kasu_derived::Ctx;
    use kasu_ops::Operations;
    use tower::ServiceExt;

    let dir = tempfile::tempdir().unwrap();
    let meta = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(FsBlobStore::open(dir.path().join("objects")).unwrap());
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
    let signature = Signature {
        name: b"Author".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: 1700000000,
        tz: b"+0000".as_slice().into(),
    };
    let id = ops
        .create_changeset(&Changeset {
            parents: vec![],
            author: signature.clone(),
            committer: signature,
            message: b"projection suffix".as_slice().into(),
            changes: [b"app.git/file".as_slice(), b"app/file"]
                .into_iter()
                .map(|path| FileChange::Set {
                    path: RepoPath::new(path).unwrap(),
                    entry: Entry::File {
                        blob: ops.ctx.repo.put_blob(path).unwrap(),
                        executable: false,
                    },
                })
                .collect(),
            git_provenance: None,
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
        0,
    )
    .unwrap();
    let app = git_http_router(Arc::new(Backend::new(
        b"repo".to_vec(),
        meta.clone(),
        blobs.clone(),
        meta,
        Some("secret".into()),
    )));
    let mut projections = Vec::new();
    for paths in [
        &[
            "path/app",
            "path/app.git",
            "path-hex/617070",
            "path-hex/617070.git",
        ][..],
        &[
            "path/app.git.git",
            "path-hex/6170702e676974",
            "path-hex/6170702e676974.git",
        ][..],
    ] {
        let mut expected = None;
        for repo in ["repo", "repo.git"] {
            for path in paths {
                let response = app
                    .clone()
                    .oneshot(
                        Request::get(format!(
                            "/{repo}/-/{path}/info/refs?service=git-upload-pack"
                        ))
                        .header("authorization", "Bearer secret")
                        .body(Body::empty())
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                if let Some(expected) = &expected {
                    assert_eq!(&body, expected, "{repo}/-/{path}");
                } else {
                    expected = Some(body);
                }
            }
        }
        projections.push(expected.unwrap());
    }
    assert_ne!(projections[0], projections[1]);
}

fn commit(path: &Path, n: u32, parents: &[GitOid]) -> GitOid {
    let data = object(path, "blob", format!("manifest {n}\n").as_bytes());
    let executable = object(path, "blob", b"#!/bin/sh\necho yes\n");
    let link = object(path, "blob", b"manifest.yaml");
    let mut raw = b"100644 manifest.yaml\0".to_vec();
    raw.extend(data.0);
    raw.extend(b"100755 run.sh\0");
    raw.extend(executable.0);
    raw.extend(b"160000 submodule\0");
    raw.extend([3; 20]);
    raw.extend(b"120000 symlink\0");
    raw.extend(link.0);
    raw.extend(b"100644 \xff\0");
    raw.extend(data.0);
    let tree = object(path, "tree", &raw);
    let mut raw = format!("tree {tree}\n").into_bytes();
    for parent in parents {
        raw.extend(format!("parent {parent}\n").bytes());
    }
    raw.extend(
        format!(
            "author A <a@b> {} +0000\ncommitter A <a@b> {} +0000\n\ncommit {n}\n",
            1_700_000_000 + n,
            1_700_000_000 + n
        )
        .bytes(),
    );
    object(path, "commit", &raw)
}
struct Running {
    _dir: tempfile::TempDir,
    source: PathBuf,
    url: String,
    state: Arc<Backend>,
    server: tokio::task::JoinHandle<()>,
    tip: GitOid,
    meta: Arc<SqliteStore>,
    blobs: Arc<FsBlobStore>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Running {
    async fn new(empty: bool) -> Self {
        Self::with_accounts(empty, Vec::new()).await
    }
    async fn with_accounts(empty: bool, accounts: Vec<ProjectionAccount>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.git");
        std::fs::create_dir_all(&source).unwrap();
        git(
            &source,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                "--object-format=sha1",
            ],
            b"",
        );
        let mut ids = Vec::new();
        if !empty {
            for n in 0..12u32 {
                let parents = if n == 0 {
                    vec![]
                } else if n == 8 {
                    vec![ids[7], ids[3]]
                } else {
                    vec![ids[n as usize - 1]]
                };
                ids.push(commit(&source, n, &parents));
            }
            git(
                &source,
                &["update-ref", "refs/heads/main", &ids[11].to_string()],
                b"",
            );
            git(
                &source,
                &["update-ref", "refs/heads/release", &ids[5].to_string()],
                b"",
            );
            let tag = object(
                &source,
                "tag",
                format!(
                    "object {}\ntype commit\ntag v1\ntagger A <a@b> 1700000012 +0000\n\nv1\n",
                    ids[11]
                )
                .as_bytes(),
            );
            git(
                &source,
                &["update-ref", "refs/tags/v1", &tag.to_string()],
                b"",
            );
        }
        let meta = Arc::new(SqliteStore::memory().unwrap());
        let blobs = Arc::new(FsBlobStore::open(dir.path().join("objects")).unwrap());
        let mut engine = Engine::standard();
        register_git(&mut engine);
        let importer = Importer {
            engine: &engine,
            index: GitIndex {
                repo: Repository {
                    meta: meta.as_ref(),
                    blobs: blobs.as_ref(),
                },
                projection: ProjectionId::full(b"repo".as_slice()),
            },
        };
        let report = importer.import(&source).unwrap();
        importer
            .publish(meta.as_ref(), &report, b"test", 0)
            .unwrap();
        let mut backend = Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            blobs.clone(),
            meta.clone(),
            Some("secret".into()),
        );
        if !accounts.is_empty() {
            backend = backend.with_projection_accounts(accounts).unwrap();
        }
        let state = Arc::new(backend);
        let bind = if std::env::var_os("KASU_GO_GIT_CONTAINER").is_some() {
            "0.0.0.0:0"
        } else {
            "127.0.0.1:0"
        };
        let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/repo.git",
            listener.local_addr().unwrap().port()
        );
        let app = http_router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _dir: dir,
            source,
            url,
            state,
            server,
            tip: ids.last().copied().unwrap_or(GitOid([0; 20])),
            meta,
            blobs,
        }
    }
}
async fn client(path: &Path, version: u8, args: Vec<String>, trace: Option<PathBuf>) -> Output {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(path)
            .args([
                "-c",
                &format!("protocol.version={version}"),
                "-c",
                "http.extraHeader=Authorization: Bearer secret",
                "-c",
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0");
        if let Some(trace) = trace {
            cmd.env("GIT_TRACE_PACKET", trace);
        }
        cmd.output().unwrap()
    })
    .await
    .unwrap()
}
fn success(out: Output) -> Vec<u8> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn missing_blobs(path: &Path) -> BTreeSet<GitOid> {
    git(
        path,
        &["rev-list", "--objects", "--all", "--missing=print"],
        b"",
    )
    .split(|b| *b == b'\n')
    .filter_map(|line| line.strip_prefix(b"?"))
    .map(|oid| oid_from_hex(oid).unwrap())
    .collect()
}

fn directory_commit(path: &Path, n: u32, parent: Option<GitOid>, content: Option<&[u8]>) -> GitOid {
    git(path, &["read-tree", "--empty"], b"");
    let secret = object(path, "blob", format!("private {n}\n").as_bytes());
    git(
        path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{secret},private/secret"),
        ],
        b"",
    );
    if let Some(content) = content {
        let blob = object(path, "blob", content);
        git(
            path,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},deploy/manifest.yaml"),
            ],
            b"",
        );
    }
    let mut tree = git(path, &["write-tree"], b"");
    if content.is_some() {
        let tree_text = std::str::from_utf8(tree.trim_ascii()).unwrap();
        let subtree = oid_from_hex(
            git(path, &["rev-parse", &format!("{tree_text}:deploy")], b"").trim_ascii(),
        )
        .unwrap();
        let mut raw = git(path, &["cat-file", "tree", tree_text], b"");
        raw.extend(b"40000 \xff\0");
        raw.extend(subtree.0);
        tree = object(path, "tree", &raw).to_string().into_bytes();
    }
    let mut raw = format!("tree {}\n", String::from_utf8(tree).unwrap().trim()).into_bytes();
    if let Some(parent) = parent {
        raw.extend(format!("parent {parent}\n").bytes());
    }
    raw.extend(
        format!(
            "author A <a@b> {} +0000\ncommitter A <a@b> {} +0000\n\nchange {n}\n",
            1700000000 + n,
            1700000000 + n
        )
        .bytes(),
    );
    object(path, "commit", &raw)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scoped_projection_pat_allows_git_but_denies_full_repository_and_native_services() {
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
    };
    use kasu_proto::v1 as p;
    use tower::ServiceExt;
    let running = Running::with_accounts(
        true,
        vec![ProjectionAccount {
            scopes: vec![],
            name: "deploy-reader".into(),
            token: "scoped-reader".into(),
            directories: vec![RepoPath::new(b"deploy".as_slice()).unwrap()],
        }],
    )
    .await;
    let first = directory_commit(&running.source, 1, None, Some(b"allowed\n"));
    reimport_directory(&running, first, 0);
    let base = running.url.strip_suffix(".git").unwrap();
    for version in 0..=2 {
        let url = format!("{base}/-/path/deploy.git");
        let destination = running._dir.path().join(format!("scoped-{version}.git"));
        // The Basic username is deliberately unrelated to the account name.
        let basic = url.replacen("http://", "http://arbitrary:scoped-reader@", 1);
        let out = Command::new("git")
            .args([
                "-c",
                &format!("protocol.version={version}"),
                "-c",
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
                "clone",
                "--bare",
                "--filter=blob:none",
                &basic,
                destination.to_str().unwrap(),
            ])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        success(out);
        let out = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args([
                "-c",
                &format!("protocol.version={version}"),
                "show",
                "HEAD:manifest.yaml",
            ])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert_eq!(success(out), b"allowed\n");
        git(&destination, &["fsck", "--strict"], b"");
    }
    for path in [
        "/repo.git/info/refs?service=git-upload-pack",
        "/repo/-/path/private.git/info/refs?service=git-upload-pack",
        "/repo/-/path/deploy-other.git/info/refs?service=git-upload-pack",
        "/repo/-/path-hex/ff.git/info/refs?service=git-upload-pack",
        "/repo/-/path/deploy.git/info/refs?service=git-receive-pack",
    ] {
        let response = http_router(running.state.clone())
            .oneshot(
                HttpRequest::get(path)
                    .header("Authorization", "Bearer scoped-reader")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    // A descendant projection is allowed; lexical siblings are not.
    let mut want = Vec::new();
    write_packet(&mut want, b"command=fetch\n").unwrap();
    want.extend(b"0001");
    write_packet(
        &mut want,
        format!("want {}\n", GitOid::hash("blob", b"private 1\n")).as_bytes(),
    )
    .unwrap();
    write_packet(&mut want, b"done\n").unwrap();
    want.extend(b"0000");
    let response = http_router(running.state.clone())
        .oneshot(
            HttpRequest::post("/repo/-/path/deploy.git/git-upload-pack")
                .header("Authorization", "Bearer scoped-reader")
                .header("Git-Protocol", "version=2")
                .header("Content-Type", "application/x-git-upload-pack-request")
                .body(Body::from(want))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = http_router(running.state.clone())
        .oneshot(
            HttpRequest::post("/repo/-/path/deploy.git/git-receive-pack")
                .header("Authorization", "Bearer scoped-reader")
                .body(Body::from("invalid framing"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = http_router(running.state.clone())
        .oneshot(
            HttpRequest::get("/repo/-/path/deploy/nested.git/info/refs?service=git-upload-pack")
                .header("Authorization", "Bearer scoped-reader")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let requests = vec![
        (
            "/v1/changeset",
            serde_json::to_vec(&p::ChangesetRequest::default()).unwrap(),
        ),
        (
            "/v1/root-tree",
            serde_json::to_vec(&p::ChangesetRequest::default()).unwrap(),
        ),
        (
            "/v1/read-file",
            serde_json::to_vec(&p::PathRequest::default()).unwrap(),
        ),
        (
            "/v1/create-changeset",
            serde_json::to_vec(&p::CreateChangesetRequest::default()).unwrap(),
        ),
        (
            "/v1/upload-blobs",
            serde_json::to_vec(&p::UploadBlobsHttpRequest::default()).unwrap(),
        ),
        (
            "/v1/land",
            serde_json::to_vec(&p::LandRequest::default()).unwrap(),
        ),
    ];
    for (path, body) in requests {
        let response = http_router(running.state.clone())
            .oneshot(
                HttpRequest::post(path)
                    .header("Authorization", "Bearer scoped-reader")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let state = running.state.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(MetadataService::new(state.clone()).into_server())
            .add_service(WriteService::new(state.clone()).into_server())
            .add_service(ObjectService::new(state).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let channel = tonic::transport::Endpoint::from_shared(endpoint)
        .unwrap()
        .connect()
        .await
        .unwrap();
    fn scoped<T>(value: T) -> tonic::Request<T> {
        let mut req = tonic::Request::new(value);
        req.metadata_mut()
            .insert("authorization", "Bearer scoped-reader".parse().unwrap());
        req
    }
    let mut metadata = p::metadata_client::MetadataClient::new(channel.clone());
    assert_eq!(
        metadata
            .get_changeset(scoped(p::ChangesetRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let mut writer = p::writer_client::WriterClient::new(channel.clone());
    assert_eq!(
        writer
            .create_changeset(scoped(p::CreateChangesetRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let mut objects = p::objects_client::ObjectsClient::new(channel);
    assert_eq!(
        objects
            .get_blobs(scoped(p::ObjectRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    assert_eq!(
        objects
            .get_trees(scoped(p::ObjectRequest::default()))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    server.abort();
}
fn reimport_directory(running: &Running, tip: GitOid, n: i64) {
    git(
        &running.source,
        &["update-ref", "refs/heads/main", &tip.to_string()],
        b"",
    );
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: running.meta.as_ref(),
                blobs: running.blobs.as_ref(),
            },
            projection: ProjectionId::full(b"repo".as_slice()),
        },
    };
    let report = importer.import(&running.source).unwrap();
    importer
        .publish(running.meta.as_ref(), &report, b"projection", n)
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_projection_clones_fetches_tags_and_isolates_source_objects() {
    let running = Running::new(true).await;
    let zero = directory_commit(&running.source, 0, None, None);
    let first = directory_commit(&running.source, 1, Some(zero), Some(b"v1\n"));
    let changed = directory_commit(&running.source, 2, Some(first), Some(b"v2\n"));
    let outside = directory_commit(&running.source, 3, Some(changed), Some(b"v2\n"));
    let tag = object(&running.source, "tag", format!("object {outside}\ntype commit\ntag v1\ntagger A <a@b> 1700000003 +0000\n\nrelease\n-----BEGIN PGP SIGNATURE-----\nsource signature\n").as_bytes());
    git(
        &running.source,
        &["update-ref", "refs/tags/v1", &tag.to_string()],
        b"",
    );
    let nested = object(
        &running.source,
        "tag",
        format!("object {tag}\ntype tag\ntag nested\ntagger A <a@b> 1700000003 +0000\n\nnested\n")
            .as_bytes(),
    );
    git(
        &running.source,
        &["update-ref", "refs/tags/nested", &nested.to_string()],
        b"",
    );
    let source_tree = git(
        &running.source,
        &["rev-parse", &format!("{outside}^{{tree}}")],
        b"",
    );
    git(
        &running.source,
        &[
            "update-ref",
            "refs/tags/tree",
            std::str::from_utf8(source_tree.trim_ascii()).unwrap(),
        ],
        b"",
    );
    git(
        &running.source,
        &[
            "update-ref",
            "refs/tags/private-blob",
            &GitOid::hash("blob", b"private 3\n").to_string(),
        ],
        b"",
    );
    reimport_directory(&running, outside, 0);
    let url = running.url.strip_suffix(".git").unwrap().to_owned() + "/-/path/deploy.git";
    let mut clones = Vec::new();
    let mut common_tip = None;
    for version in 0..=2 {
        for mode in ["full", "depth1", "partial"] {
            let dest = running
                ._dir
                .path()
                .join(format!("projection-{version}-{mode}.git"));
            let trace = running
                ._dir
                .path()
                .join(format!("projection-{version}-{mode}.trace"));
            let mut args = vec![
                "clone".into(),
                "--bare".into(),
                "--config=transfer.fsckObjects=true".into(),
            ];
            if mode == "depth1" {
                args.push("--depth=1".into());
            }
            if mode == "partial" {
                args.push("--filter=blob:none".into());
            }
            args.extend([url.clone(), dest.to_str().unwrap().into()]);
            success(client(running._dir.path(), version, args, Some(trace.clone())).await);
            let tip = git(&dest, &["rev-parse", "HEAD"], b"");
            if let Some(expected) = &common_tip {
                assert_eq!(&tip, expected);
            } else {
                common_tip = Some(tip.clone());
            }
            assert_ne!(tip.trim_ascii(), outside.to_string().as_bytes());
            assert_eq!(
                success(
                    client(
                        &dest,
                        version,
                        vec!["show".into(), "HEAD:manifest.yaml".into()],
                        Some(trace.clone())
                    )
                    .await
                ),
                b"v2\n"
            );
            assert_eq!(
                git(&dest, &["ls-tree", "--name-only", "HEAD"], b""),
                b"manifest.yaml\n"
            );
            assert_eq!(
                git(&dest, &["rev-list", "--count", "HEAD"], b""),
                if mode == "depth1" { b"1\n" } else { b"2\n" }
            );
            assert_eq!(git(&dest, &["rev-parse", "v1^{}"], b""), tip);
            assert_eq!(git(&dest, &["rev-parse", "nested^{}"], b""), tip);
            if mode != "depth1" {
                assert_eq!(
                    git(&dest, &["ls-tree", "--name-only", "tree"], b""),
                    b"manifest.yaml\n"
                );
            }
            assert!(
                !String::from_utf8_lossy(&git(&dest, &["tag", "--list"], b""))
                    .contains("private-blob")
            );
            assert!(
                !String::from_utf8_lossy(&git(&dest, &["cat-file", "tag", "v1"], b""))
                    .contains("SIGNATURE")
            );
            for forbidden in [outside, GitOid::hash("blob", b"private 3\n")] {
                let result = client(
                    &dest,
                    version,
                    vec!["fetch".into(), "origin".into(), forbidden.to_string()],
                    Some(trace.clone()),
                )
                .await;
                assert!(
                    !result.status.success(),
                    "source object leaked: {forbidden}"
                );
            }
            let refused = client(
                &dest,
                version,
                vec![
                    "push".into(),
                    "origin".into(),
                    "HEAD:refs/heads/wrong".into(),
                ],
                Some(trace.clone()),
            )
            .await;
            assert!(!refused.status.success());
            git(&dest, &["fsck", "--strict"], b"");
            clones.push((version, mode, dest, trace));
        }
    }
    let hex_url =
        running.url.strip_suffix(".git").unwrap().to_owned() + "/-/path-hex/6465706c6f79.git";
    assert_eq!(
        success(
            client(
                running._dir.path(),
                2,
                vec!["ls-remote".into(), url.clone()],
                None
            )
            .await
        ),
        success(
            client(
                running._dir.path(),
                2,
                vec!["ls-remote".into(), hex_url],
                None
            )
            .await
        )
    );
    let byte_url = running.url.strip_suffix(".git").unwrap().to_owned() + "/-/path-hex/ff.git";
    let byte_dest = running._dir.path().join("byte-projection.git");
    success(
        client(
            running._dir.path(),
            2,
            vec![
                "clone".into(),
                "--bare".into(),
                byte_url,
                byte_dest.to_str().unwrap().into(),
            ],
            None,
        )
        .await,
    );
    assert_eq!(
        git(&byte_dest, &["show", "HEAD:manifest.yaml"], b""),
        b"v2\n"
    );
    assert_ne!(
        git(&byte_dest, &["rev-parse", "HEAD"], b""),
        *common_tip.as_ref().unwrap()
    );
    let other_tip = std::str::from_utf8(common_tip.as_ref().unwrap().trim_ascii())
        .unwrap()
        .to_owned();
    assert!(
        !client(
            &byte_dest,
            2,
            vec!["fetch".into(), "origin".into(), other_tip],
            None
        )
        .await
        .status
        .success()
    );
    git(&byte_dest, &["fsck", "--strict"], b"");
    let outside_again = directory_commit(&running.source, 4, Some(outside), Some(b"v2\n"));
    reimport_directory(&running, outside_again, 1);
    for (version, _, dest, trace) in &clones {
        success(
            client(
                dest,
                *version,
                vec![
                    "fetch".into(),
                    "origin".into(),
                    "refs/heads/main:refs/heads/main".into(),
                ],
                Some(trace.clone()),
            )
            .await,
        );
        assert_eq!(
            git(dest, &["rev-parse", "HEAD"], b""),
            *common_tip.as_ref().unwrap()
        );
    }
    let next = directory_commit(&running.source, 5, Some(outside_again), Some(b"v3\n"));
    reimport_directory(&running, next, 2);
    for (version, mode, dest, trace) in &clones {
        success(
            client(
                dest,
                *version,
                vec![
                    "fetch".into(),
                    "origin".into(),
                    "refs/heads/main:refs/heads/main".into(),
                ],
                Some(trace.clone()),
            )
            .await,
        );
        assert_ne!(
            git(dest, &["rev-parse", "HEAD"], b""),
            *common_tip.as_ref().unwrap()
        );
        assert_eq!(
            success(
                client(
                    dest,
                    *version,
                    vec!["show".into(), "HEAD:manifest.yaml".into()],
                    Some(trace.clone())
                )
                .await
            ),
            b"v3\n"
        );
        git(dest, &["fsck", "--strict"], b"");
        if let Some(capture) = std::env::var_os("KASU_CAPTURE_DIR") {
            std::fs::create_dir_all(&capture).unwrap();
            let git_version = String::from_utf8(git(dest, &["--version"], b"")).unwrap();
            let packets = std::fs::read_to_string(trace)
                .unwrap()
                .lines()
                .filter_map(|line| line.split_once("packet: ").map(|(_, data)| data.to_owned()))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(
                PathBuf::from(capture).join(format!(
                    "git-{}-http-v{version}-path-{mode}.trace",
                    git_version.split_whitespace().nth(2).unwrap()
                )),
                format!("# {}\n{packets}\n", git_version.trim()),
            )
            .unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_clone_omits_blobs_and_promisor_fetch_hydrates_history_and_sparse_checkout() {
    let running = Running::new(false).await;
    for version in 0..=2 {
        for shallow in [false, true] {
            let dest = running
                ._dir
                .path()
                .join(format!("partial-{version}-{shallow}"));
            let trace = running
                ._dir
                .path()
                .join(format!("partial-{version}-{shallow}.trace"));
            let mut args = vec![
                "clone".into(),
                "--config=transfer.fsckObjects=true".into(),
                "--filter=blob:none".into(),
                "--no-checkout".into(),
            ];
            if shallow {
                args.push("--depth=1".into());
            }
            args.extend([running.url.clone(), dest.to_str().unwrap().into()]);
            success(client(running._dir.path(), version, args, Some(trace.clone())).await);
            assert_eq!(
                git(&dest, &["config", "remote.origin.promisor"], b""),
                b"true\n"
            );
            let missing = missing_blobs(&dest);
            assert_eq!(missing.len(), if shallow { 3 } else { 14 });
            // Enumerating the physical object database cannot trigger lazy fetch.
            let stored = git(
                &dest,
                &[
                    "cat-file",
                    "--batch-all-objects",
                    "--batch-check=%(objecttype)",
                ],
                b"",
            );
            assert!(!stored.split(|b| *b == b'\n').any(|kind| kind == b"blob"));
            git(&dest, &["fsck", "--strict"], b"");

            let revision = if shallow {
                "HEAD:manifest.yaml"
            } else {
                "HEAD~10:manifest.yaml"
            };
            let expected = git(&running.source, &["show", revision], b"");
            let oid =
                oid_from_hex(git(&running.source, &["rev-parse", revision], b"").trim_ascii())
                    .unwrap();
            assert!(missing.contains(&oid));
            assert_eq!(
                success(
                    client(
                        &dest,
                        version,
                        vec!["cat-file".into(), "blob".into(), oid.to_string()],
                        Some(trace.clone())
                    )
                    .await
                ),
                expected
            );
            assert!(!missing_blobs(&dest).contains(&oid));

            git(
                &dest,
                &[
                    "sparse-checkout",
                    "set",
                    "--no-cone",
                    "/manifest.yaml",
                    "/run.sh",
                    "/symlink",
                ],
                b"",
            );
            success(
                client(
                    &dest,
                    version,
                    vec!["checkout".into(), "--force".into(), "HEAD".into()],
                    Some(trace.clone()),
                )
                .await,
            );
            assert_eq!(
                std::fs::read(dest.join("manifest.yaml")).unwrap(),
                b"manifest 11\n"
            );
            assert_eq!(
                std::fs::read_link(dest.join("symlink")).unwrap(),
                Path::new("manifest.yaml")
            );
            assert!(!dest.join("submodule").exists());
            // Deepening must preserve the filter, including blobs reachable only
            // from intermediate history that was not sent in the shallow clone.
            if shallow {
                success(
                    client(
                        &dest,
                        version,
                        vec!["fetch".into(), "--unshallow".into(), "origin".into()],
                        Some(trace.clone()),
                    )
                    .await,
                );
            }
            let remaining = missing_blobs(&dest);
            assert!(!remaining.is_empty());
            let mut args = vec!["fetch".into(), "--no-tags".into(), "origin".into()];
            args.extend(remaining.iter().map(ToString::to_string));
            success(client(&dest, version, args, Some(trace.clone())).await);
            assert!(missing_blobs(&dest).is_empty());
            git(&dest, &["fsck", "--strict"], b"");
            let all = git(
                &dest,
                &["rev-list", "--objects", "HEAD", "--no-object-names"],
                b"",
            );
            assert_eq!(
                all,
                git(
                    &running.source,
                    &["rev-list", "--objects", "HEAD", "--no-object-names"],
                    b""
                )
            );
            if let Some(capture) = std::env::var_os("KASU_CAPTURE_DIR") {
                std::fs::create_dir_all(&capture).unwrap();
                let git_version = String::from_utf8(git(&dest, &["--version"], b"")).unwrap();
                let lines = std::fs::read_to_string(trace)
                    .unwrap()
                    .lines()
                    .filter_map(|line| line.split_once("packet: ").map(|(_, data)| data.to_owned()))
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(
                    PathBuf::from(capture).join(format!(
                        "git-{}-http-v{version}-partial-{}.trace",
                        git_version.split_whitespace().nth(2).unwrap(),
                        if shallow { "depth1" } else { "full" }
                    )),
                    format!("# {}\n{lines}\n", git_version.trim()),
                )
                .unwrap();
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_clone_incremental_fetch_keeps_promises_across_common_commits() {
    for version in 0..=2 {
        let running = Running::new(false).await;
        let dest = running._dir.path().join("incremental.git");
        success(
            client(
                running._dir.path(),
                version,
                vec![
                    "clone".into(),
                    "--bare".into(),
                    "--filter=blob:none".into(),
                    running.url.clone(),
                    dest.to_str().unwrap().into(),
                ],
                None,
            )
            .await,
        );
        let initial = missing_blobs(&dest);
        let mut tip = running.tip;
        for n in 12..15 {
            tip = commit(&running.source, n, &[tip]);
            git(
                &running.source,
                &["update-ref", "refs/heads/main", &tip.to_string()],
                b"",
            );
            let mut engine = Engine::standard();
            register_git(&mut engine);
            let importer = Importer {
                engine: &engine,
                index: GitIndex {
                    repo: Repository {
                        meta: running.meta.as_ref(),
                        blobs: running.blobs.as_ref(),
                    },
                    projection: ProjectionId::full(b"repo".as_slice()),
                },
            };
            let report = importer.import(&running.source).unwrap();
            importer
                .publish(running.meta.as_ref(), &report, b"partial", n.into())
                .unwrap();
            success(
                client(
                    &dest,
                    version,
                    vec![
                        "fetch".into(),
                        "origin".into(),
                        "refs/heads/main:refs/heads/main".into(),
                    ],
                    None,
                )
                .await,
            );
            let missing = missing_blobs(&dest);
            assert!(initial.is_subset(&missing));
            assert_eq!(missing.len(), initial.len() + (n - 11) as usize);
            git(&dest, &["fsck", "--strict"], b"");
        }
        let promised = *initial.first().unwrap();
        assert_eq!(
            success(
                client(
                    &dest,
                    version,
                    vec!["cat-file".into(), "blob".into(), promised.to_string()],
                    None
                )
                .await
            ),
            git(
                &running.source,
                &["cat-file", "blob", &promised.to_string()],
                b""
            )
        );
        assert!(!missing_blobs(&dest).contains(&promised));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_versions_ls_remote_full_clone_and_depth_one() {
    let running = Running::new(false).await;
    for version in 0..=2 {
        let remote = success(
            client(
                running._dir.path(),
                version,
                vec!["ls-remote".into(), running.url.clone()],
                None,
            )
            .await,
        );
        let expected = git(
            &running.source,
            &["ls-remote", running.source.to_str().unwrap()],
            b"",
        );
        assert_eq!(remote, expected, "ls-remote v{version}");
        for depth in [false, true] {
            let dest = running
                ._dir
                .path()
                .join(format!("v{version}-depth{depth}.git"));
            let trace = running
                ._dir
                .path()
                .join(format!("v{version}-{depth}.trace"));
            let mut args = vec!["clone".into(), "--bare".into()];
            if depth {
                args.push("--depth=1".into());
            }
            args.extend([running.url.clone(), dest.to_str().unwrap().into()]);
            let result = client(running._dir.path(), version, args, Some(trace.clone())).await;
            if let Some(capture) = std::env::var_os("KASU_CAPTURE_DIR") {
                std::fs::create_dir_all(&capture).unwrap();
                let git_version =
                    String::from_utf8(git(running._dir.path(), &["--version"], b"")).unwrap();
                let text = std::fs::read_to_string(&trace).unwrap();
                let lines = text
                    .lines()
                    .filter_map(|l| l.split_once("packet: ").map(|(_, line)| line))
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(
                    PathBuf::from(capture).join(format!(
                        "git-{}-http-v{version_number}-{}.trace",
                        git_version.split_whitespace().nth(2).unwrap(),
                        if depth { "depth1" } else { "full" },
                        version_number = version
                    )),
                    format!("# {}\n{}\n", git_version.trim(), lines),
                )
                .unwrap();
            }
            if !result.status.success() {
                eprintln!("{}", std::fs::read_to_string(&trace).unwrap());
            }
            success(result);
            assert_eq!(
                git(&dest, &["rev-parse", "HEAD"], b"").trim_ascii(),
                running.tip.to_string().as_bytes()
            );
            git(&dest, &["fsck", "--strict"], b"");
            if depth {
                assert_eq!(git(&dest, &["rev-list", "--count", "HEAD"], b""), b"1\n");
                assert_eq!(
                    git(&dest, &["rev-parse", "--is-shallow-repository"], b""),
                    b"true\n"
                );
            } else {
                let mut actual = git(
                    &dest,
                    &["rev-list", "--objects", "--all", "--no-object-names"],
                    b"",
                )
                .split(|b| *b == b'\n')
                .map(|l| l.to_vec())
                .collect::<Vec<_>>();
                let mut expected = git(
                    &running.source,
                    &["rev-list", "--objects", "--all", "--no-object-names"],
                    b"",
                )
                .split(|b| *b == b'\n')
                .map(|l| l.to_vec())
                .collect::<Vec<_>>();
                actual.sort();
                expected.sort();
                assert_eq!(actual, expected);
            }
        }
    }
    let stats = running.state.git_cache_stats();
    assert!(stats.hits > 0);
    assert!(stats.bytes <= 32 * 1024 * 1024);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_repository_and_basic_auth_and_incremental_fetch() {
    let empty = Running::new(true).await;
    success(
        client(
            empty._dir.path(),
            2,
            vec!["ls-remote".into(), empty.url.clone()],
            None,
        )
        .await,
    );
    let dest = empty._dir.path().join("empty-clone");
    success(
        client(
            empty._dir.path(),
            2,
            vec![
                "clone".into(),
                empty.url.clone(),
                dest.to_str().unwrap().into(),
            ],
            None,
        )
        .await,
    );
    assert_eq!(
        git(&dest, &["symbolic-ref", "HEAD"], b""),
        b"refs/heads/main\n"
    );
    let running = Running::new(false).await;
    let basic = running.url.replacen("http://", "http://user:secret@", 1);
    let url = basic.clone();
    let out = tokio::task::spawn_blocking(move || {
        Command::new("git")
            .args(["ls-remote", &url])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    success(out);
    let mut tip = running.tip;
    for version in 0..=2 {
        let clone = running
            ._dir
            .path()
            .join(format!("incremental-{version}.git"));
        success(
            client(
                running._dir.path(),
                version,
                vec![
                    "clone".into(),
                    "--bare".into(),
                    running.url.clone(),
                    clone.to_str().unwrap().into(),
                ],
                None,
            )
            .await,
        );
        let next = commit(&running.source, 12 + version as u32, &[tip]);
        git(
            &running.source,
            &["update-ref", "refs/heads/main", &next.to_string()],
            b"",
        );
        let mut engine = Engine::standard();
        register_git(&mut engine);
        let importer = Importer {
            engine: &engine,
            index: GitIndex {
                repo: Repository {
                    meta: running.meta.as_ref(),
                    blobs: running.blobs.as_ref(),
                },
                projection: ProjectionId::full(b"repo".as_slice()),
            },
        };
        let report = importer.import(&running.source).unwrap();
        importer
            .publish(running.meta.as_ref(), &report, b"update", 1)
            .unwrap();
        success(
            client(
                &clone,
                version,
                vec![
                    "fetch".into(),
                    "origin".into(),
                    "refs/heads/main:refs/heads/main".into(),
                ],
                None,
            )
            .await,
        );
        git(&clone, &["fsck", "--strict"], b"");
        assert_eq!(
            git(&clone, &["rev-parse", "HEAD"], b"").trim_ascii(),
            next.to_string().as_bytes()
        );
        tip = next;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires tools/git-compat binary in KASU_GO_GIT_CLIENT"]
async fn pinned_go_git_full_and_shallow_checkout() {
    let running = Running::new(false).await;
    let binary =
        PathBuf::from(std::env::var_os("KASU_GO_GIT_CLIENT").expect("set KASU_GO_GIT_CLIENT"));
    for depth in [0, 1] {
        let dest = running._dir.path().join(format!("go-git-{depth}"));
        let url = running.url.clone();
        let bin = binary.clone();
        let dst = dest.clone();
        let out = tokio::task::spawn_blocking(move || {
            Command::new(bin)
                .args([url, dst.to_str().unwrap().into(), depth.to_string()])
                .env("KASU_TOKEN", "secret")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        if let Some(capture) = std::env::var_os("KASU_CAPTURE_DIR") {
            std::fs::create_dir_all(&capture).unwrap();
            std::fs::write(
                PathBuf::from(capture).join(format!("go-git-5.19.3-http-depth{depth}.trace")),
                &out.stderr,
            )
            .unwrap();
        }
        let stdout = success(out);
        assert_eq!(stdout.trim_ascii(), running.tip.to_string().as_bytes());
        git(&dest, &["fsck", "--strict"], b"");
        assert_eq!(
            std::fs::read(dest.join("manifest.yaml")).unwrap(),
            b"manifest 11\n"
        );
        assert_eq!(
            std::fs::read_link(dest.join("symlink")).unwrap(),
            Path::new("manifest.yaml")
        );
        if depth == 1 {
            assert_eq!(git(&dest, &["rev-list", "--count", "HEAD"], b""), b"1\n");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shallow_deepen_unshallow_and_v2_time_and_exclusion() {
    let running = Running::new(false).await;
    for version in 0..=2 {
        let dest = running._dir.path().join(format!("deepen-{version}.git"));
        success(
            client(
                running._dir.path(),
                version,
                vec![
                    "clone".into(),
                    "--bare".into(),
                    "--depth=1".into(),
                    running.url.clone(),
                    dest.to_str().unwrap().into(),
                ],
                None,
            )
            .await,
        );
        success(
            client(
                &dest,
                version,
                vec!["fetch".into(), "--deepen=2".into(), "origin".into()],
                None,
            )
            .await,
        );
        git(&dest, &["fsck", "--strict"], b"");
        assert_eq!(git(&dest, &["rev-list", "--count", "HEAD"], b""), b"3\n");
        success(
            client(
                &dest,
                version,
                vec!["fetch".into(), "--unshallow".into(), "origin".into()],
                None,
            )
            .await,
        );
        git(&dest, &["fsck", "--strict"], b"");
        assert_eq!(
            git(&dest, &["rev-parse", "--is-shallow-repository"], b""),
            b"false\n"
        );
        assert_eq!(git(&dest, &["rev-list", "--count", "HEAD"], b""), b"12\n");
    }
    for version in 0..=2 {
        for option in [
            "--shallow-since=2023-11-14T22:13:28Z",
            "--shallow-exclude=release",
        ] {
            let dest = running._dir.path().join(format!(
                "boundary-{version}-{}",
                if option.contains("since") { 0 } else { 1 }
            ));
            success(
                client(
                    running._dir.path(),
                    version,
                    vec![
                        "clone".into(),
                        "--bare".into(),
                        option.into(),
                        running.url.clone(),
                        dest.to_str().unwrap().into(),
                    ],
                    None,
                )
                .await,
            );
            git(&dest, &["fsck", "--strict"], b"");
            assert_eq!(git(&dest, &["rev-list", "--count", "HEAD"], b""), b"4\n");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreferenced_wants_require_explicit_publication_and_auth_precedes_parsing() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let running = Running::new(false).await;
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let repo = Repository {
        meta: running.meta.as_ref(),
        blobs: running.blobs.as_ref(),
    };
    let signature = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: 1,
        tz: b"+0000".as_slice().into(),
    };
    let blob = repo.put_blob(b"private\n").unwrap();
    let cs = Changeset {
        parents: vec![],
        author: signature.clone(),
        committer: signature,
        message: b"private".as_slice().into(),
        changes: vec![FileChange::Set {
            path: RepoPath::new(b"private".as_slice()).unwrap(),
            entry: Entry::File {
                blob,
                executable: false,
            },
        }],
        git_provenance: None,
    };
    let id = repo.put_changeset(&cs).unwrap();
    let ctx = kasu_derived::Ctx {
        repo: Repository {
            meta: running.meta.as_ref(),
            blobs: running.blobs.as_ref(),
        },
    };
    let other = ProjectionId::full(b"other".as_slice());
    let oid = engine.derive::<GitCommit>(&ctx, id, &other).unwrap().0;
    fn body(oid: GitOid) -> Vec<u8> {
        let mut out = Vec::new();
        write_packet(&mut out, b"command=fetch\n").unwrap();
        out.extend(b"0001");
        write_packet(&mut out, format!("want {oid}\n").as_bytes()).unwrap();
        write_packet(&mut out, b"done\n").unwrap();
        out.extend(b"0000");
        out
    }
    fn request(bytes: Vec<u8>, authenticated: bool) -> Request<Body> {
        let mut req = Request::post("/repo.git/git-upload-pack")
            .header("Git-Protocol", "version=2")
            .header("Content-Type", "application/x-git-upload-pack-request");
        if authenticated {
            req = req.header("Authorization", "Bearer secret");
        }
        req.body(Body::from(bytes)).unwrap()
    }
    let response = http_router(running.state.clone())
        .oneshot(request(b"not framing".to_vec(), false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key("www-authenticate"));
    let response = http_router(running.state.clone())
        .oneshot(request(body(oid), true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let projection = ProjectionId::full(b"repo".as_slice());
    engine.derive::<GitCommit>(&ctx, id, &projection).unwrap();
    let response = http_router(running.state.clone())
        .oneshot(request(body(oid), true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let index = GitIndex {
        repo: ctx.repo,
        projection,
    };
    index.publish_commit(oid).unwrap();
    let response = http_router(running.state.clone())
        .oneshot(request(body(oid), true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let packets = parse_packets(&bytes).unwrap();
    let mut pack = Vec::new();
    for packet in packets {
        if let Packet::Data(bytes) = packet
            && bytes.first() == Some(&1)
        {
            pack.extend_from_slice(&bytes[1..]);
        }
    }
    let destination = running._dir.path().join("published.git");
    std::fs::create_dir_all(&destination).unwrap();
    git(
        &destination,
        &["init", "--bare", "--object-format=sha1"],
        b"",
    );
    git(&destination, &["index-pack", "--stdin", "--strict"], &pack);
    let response = http_router(running.state.clone())
        .oneshot(
            Request::get("/repo.git/info/refs?service=git-receive-pack")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/x-git-receive-pack-advertisement"
    );
}
