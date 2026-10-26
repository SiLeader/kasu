use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use kasu_core::*;
use kasu_derived::*;
use kasu_git::*;
use kasu_ingest::Importer;
use kasu_proto::v1::{self as p, metadata_client::MetadataClient};
use kasu_server::*;
use kasu_store::*;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};
use tower::ServiceExt;

fn git(path: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    result.stdout
}
fn object(path: &Path, kind: &str, raw: &[u8]) -> GitOid {
    oid_from_hex(git(path, &["hash-object", "-w", "-t", kind, "--stdin"], raw).trim_ascii())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_move_and_copy_fragment_blame_match_git_over_native_grpc_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("lineage.git");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "--bare", "--initial-branch=main"], b"");
    let text=b"one long original line\ntwo long original line\nthree long original line\nfour long original line\nfive long original line\n";
    let changed=b"one long original line\ntwo changed new line\nthree long original line\nfour long original line\nfive long original line\n";
    let edited=b"one long original line\ntwo changed new line\nthree long original line\nfour edited copy line\nfive long original line\n";
    let section_a = b"alpha original function with enough letters for tracking\nalpha body returns an original alpha value\n";
    let section_b = b"beta independent function with enough letters for tracking\nbeta body returns an original beta value\n";
    let fragment_original = [section_a.as_slice(), section_b.as_slice()].concat();
    let fragment_moved = [section_b.as_slice(), section_a.as_slice()].concat();
    let fragment_header = b"unique destination header for fragment copy testing\n";
    let fragment_copy = [fragment_header.as_slice(), section_a.as_slice()].concat();
    let fragment_edit = [
        fragment_header.as_slice(),
        section_a.split_inclusive(|b| *b == b'\n').next().unwrap(),
        b"new edit in copied function\n".as_slice(),
    ]
    .concat();
    let new_copy = [
        b"new copy header\n".as_slice(),
        section_b.as_slice(),
        b"new copy footer\n".as_slice(),
    ]
    .concat();
    let mut oids = Vec::new();
    for (n, files) in [
        vec![("old", text.as_slice())],
        vec![("renamed", text.as_slice())],
        vec![("again", changed.as_slice())],
        vec![("again", changed.as_slice()), ("copy", changed.as_slice())],
        vec![("again", changed.as_slice()), ("copy", edited.as_slice())],
        vec![
            ("again", changed.as_slice()),
            ("copy", fragment_header.as_slice()),
            ("fragments", fragment_original.as_slice()),
        ],
        vec![
            ("again", changed.as_slice()),
            ("copy", fragment_header.as_slice()),
            ("fragments", fragment_moved.as_slice()),
        ],
        vec![
            ("again", changed.as_slice()),
            ("copy", fragment_copy.as_slice()),
            ("fragments", fragment_moved.as_slice()),
        ],
        vec![
            ("again", changed.as_slice()),
            ("copy", fragment_edit.as_slice()),
            ("fragments", fragment_moved.as_slice()),
        ],
        vec![
            ("again", changed.as_slice()),
            ("copy", fragment_edit.as_slice()),
            ("fragments", fragment_moved.as_slice()),
            ("newcopy", new_copy.as_slice()),
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let mut raw = Vec::new();
        for (name, bytes) in files {
            raw.extend(format!("100644 {name}\0").bytes());
            raw.extend(object(&source, "blob", bytes).0);
        }
        let tree = object(&source, "tree", &raw);
        let mut raw = format!("tree {tree}\n").into_bytes();
        if let Some(parent) = oids.last() {
            raw.extend(format!("parent {parent}\n").bytes());
        }
        raw.extend(
            format!(
                "author A <a@b> {} +0000\ncommitter A <a@b> {} +0000\n\nlineage {n}\n",
                n + 1,
                n + 1
            )
            .bytes(),
        );
        oids.push(object(&source, "commit", &raw));
    }
    git(
        &source,
        &["update-ref", "refs/heads/main", &oids[9].to_string()],
        b"",
    );
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
        .publish(meta.as_ref(), &report, b"lineage", 0)
        .unwrap();
    let ids: Vec<_> = oids
        .iter()
        .map(|oid| importer.index.find_changeset(*oid).unwrap().unwrap())
        .collect();
    let backend = Arc::new(Backend::new(
        b"repo".to_vec(),
        meta.clone(),
        blobs,
        meta,
        Some("secret".into()),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let state = backend.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(MetadataService::new(state).into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let mut client = MetadataClient::connect(endpoint).await.unwrap();
    let mut cases = Vec::new();
    for (n, path) in [
        (1, "renamed"),
        (2, "again"),
        (3, "copy"),
        (4, "copy"),
        (6, "fragments"),
        (7, "copy"),
        (8, "copy"),
        (9, "newcopy"),
    ] {
        for (follow_moves, follow_copies) in [(false, false), (true, false), (false, true)] {
            let q = p::PathRequest {
                repo: b"repo".to_vec(),
                id: ids[n].0.to_vec(),
                path: path.as_bytes().to_vec(),
                follow_copies,
                follow_moves,
            };
            let actual = backend.blame(q.clone()).unwrap();
            let mut args = vec!["blame", "--line-porcelain"];
            if follow_moves {
                args.push("-M");
            }
            if follow_copies {
                if n < 5 {
                    args.extend(["-C1", "-C1"]);
                } else {
                    args.extend(["-M", "-C", "-C", "-C"]);
                }
            }
            let oid = oids[n].to_string();
            args.extend([&oid, "--", path]);
            let output = git(&source, &args, b"");
            let mut expected = Vec::new();
            let mut origin = None;
            let mut filename = Vec::new();
            for line in output.split(|b| *b == b'\n') {
                let fields: Vec<_> = line.split(|b| *b == b' ').collect();
                if fields.len() >= 3 && fields[0].len() == 40 {
                    origin = Some((
                        oid_from_hex(fields[0]).unwrap(),
                        std::str::from_utf8(fields[1])
                            .unwrap()
                            .parse::<u64>()
                            .unwrap(),
                    ));
                }
                if let Some(path) = line.strip_prefix(b"filename ") {
                    filename = path.to_vec();
                }
                if line.starts_with(b"\t") {
                    let (oid, line) = origin.unwrap();
                    expected.push((oid, line, filename.clone()));
                }
            }
            let tuples: Vec<_> = actual
                .lines
                .iter()
                .map(|line| {
                    let at = ids
                        .iter()
                        .position(|id| id.0.as_slice() == line.changeset)
                        .unwrap();
                    (oids[at], line.line, line.path.clone())
                })
                .collect();
            assert_eq!(
                tuples, expected,
                "n={n} copies={follow_copies} moves={follow_moves}"
            );
            let mut request = tonic::Request::new(q.clone());
            request
                .metadata_mut()
                .insert("authorization", "Bearer secret".parse().unwrap());
            assert_eq!(client.blame(request).await.unwrap().into_inner(), actual);
            let response = http_router(backend.clone())
                .oneshot(
                    Request::post("/v1/blame")
                        .header("Authorization", "Bearer secret")
                        .header("Content-Type", "application/json")
                        .body(Body::from(serde_json::to_vec(&q).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                serde_json::from_slice::<p::BlameResponse>(&body).unwrap(),
                actual
            );
            cases.push(serde_json::json!({"revision":n,"path":path,"follow_copies":follow_copies,"follow_moves":follow_moves,"origins":actual.lines}));
        }
    }
    server.abort();
    if let Some(capture) = std::env::var_os("KASU_CAPTURE_DIR") {
        std::fs::create_dir_all(&capture).unwrap();
        let version = String::from_utf8(git(&source, &["--version"], b"")).unwrap();
        let name = format!(
            "blame-lineage-git-{}.json",
            version.split_whitespace().nth(2).unwrap()
        );
        std::fs::write(
            PathBuf::from(capture).join(name),
            serde_json::to_vec_pretty(&serde_json::json!({"git":version.trim(),"cases":cases}))
                .unwrap(),
        )
        .unwrap();
    }
}
fn tree(path: &Path, content: Option<&[u8]>, executable: bool) -> GitOid {
    let mut raw = Vec::new();
    if let Some(content) = content {
        raw.extend(if executable { b"100755" } else { b"100644" });
        raw.extend(b" f\0");
        raw.extend(object(path, "blob", content).0);
    }
    raw.extend(b"100644 other\0");
    raw.extend(object(path, "blob", b"untouched\xff\n").0);
    raw.extend(b"100644 \xff\0");
    raw.extend(object(path, "blob", b"byte path\n").0);
    object(path, "tree", &raw)
}
fn commit(
    path: &Path,
    content: Option<&[u8]>,
    executable: bool,
    parents: &[GitOid],
    n: usize,
) -> GitOid {
    let tree = tree(path, content, executable);
    let mut raw = format!("tree {tree}\n").into_bytes();
    for p in parents {
        raw.extend(format!("parent {p}\n").bytes());
    }
    raw.extend(
        format!(
            "author A <a@b> {} +0000\ncommitter A <a@b> {} +0000\n\ncommit {n}\n",
            n + 1,
            n + 1
        )
        .bytes(),
    );
    object(path, "commit", &raw)
}
struct Fixture {
    _dir: tempfile::TempDir,
    source: PathBuf,
    meta: Arc<SqliteStore>,
    blobs: Arc<FsBlobStore>,
    backend: Arc<Backend>,
    oids: Vec<GitOid>,
    ids: Vec<ChangesetId>,
}
impl Fixture {
    fn new() -> Self {
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
        let a = b"a\nsame\nx\nsame\nz\n";
        let left = b"a\nleft\nx\nsame\nz\n";
        let right = b"a\nsame\nx\nright\nz\n";
        let merged = b"a\nleft\nx\nright\nz\n";
        let root = commit(&source, Some(a), false, &[], 0);
        let l = commit(&source, Some(left), false, &[root], 1);
        let r = commit(&source, Some(right), false, &[root], 2);
        let m = commit(&source, Some(merged), false, &[l, r], 3);
        let mode = commit(&source, Some(merged), true, &[m], 4);
        let deleted = commit(&source, None, false, &[mode], 5);
        let restored = commit(&source, Some(b"restored\nno newline"), false, &[deleted], 6);
        let x = commit(&source, Some(merged), false, &[l, r], 7);
        let y = commit(&source, Some(merged), false, &[r, l], 8);
        for (name, oid) in [
            ("main", restored),
            ("left", l),
            ("right", r),
            ("x", x),
            ("y", y),
        ] {
            git(
                &source,
                &[
                    "update-ref",
                    &format!("refs/heads/{name}"),
                    &oid.to_string(),
                ],
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
        let oids = vec![root, l, r, m, mode, deleted, restored, x, y];
        let ids = oids
            .iter()
            .map(|oid| importer.index.find_changeset(*oid).unwrap().unwrap())
            .collect();
        let backend = Arc::new(Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            blobs.clone(),
            meta.clone(),
            Some("secret".into()),
        ));
        Self {
            _dir: dir,
            source,
            meta,
            blobs,
            backend,
            oids,
            ids,
        }
    }
    fn request(&self, n: usize) -> p::ChangesetRequest {
        p::ChangesetRequest {
            repo: b"repo".to_vec(),
            id: self.ids[n].0.to_vec(),
        }
    }
    fn path(&self, n: usize, path: &[u8]) -> p::PathRequest {
        p::PathRequest {
            follow_copies: false,
            follow_moves: false,
            repo: b"repo".to_vec(),
            id: self.ids[n].0.to_vec(),
            path: path.to_vec(),
        }
    }
    fn oid(&self, id: &[u8]) -> GitOid {
        let n = self.ids.iter().position(|x| x.0.as_slice() == id).unwrap();
        self.oids[n]
    }
}
#[test]
fn path_log_blame_diff_and_all_merge_bases_match_git() {
    let f = Fixture::new();
    for n in 0..f.ids.len() {
        for path in [b"f".as_slice(), b"other", b"missing"] {
            let mut cursor = Vec::new();
            let mut actual = Vec::new();
            loop {
                let result = f
                    .backend
                    .log(p::LogRequest {
                        repo: b"repo".to_vec(),
                        start: f.ids[n].0.to_vec(),
                        path: Some(path.to_vec()),
                        limit: 1,
                        cursor,
                    })
                    .unwrap();
                actual.extend(result.changesets.iter().map(|cs| f.oid(&cs.id).to_string()));
                cursor = result.cursor;
                if cursor.is_empty() {
                    break;
                }
            }
            let expected = String::from_utf8(git(
                &f.source,
                &[
                    "log",
                    "--first-parent",
                    "--format=%H",
                    &f.oids[n].to_string(),
                    "--",
                    std::str::from_utf8(path).unwrap(),
                ],
                b"",
            ))
            .unwrap();
            assert_eq!(
                actual,
                expected.lines().map(str::to_owned).collect::<Vec<_>>(),
                "path log n={n} path={path:?}"
            );
            if path == b"missing" || (path == b"f" && n == 5) {
                assert_eq!(
                    f.backend.blame(f.path(n, path)).unwrap_err().code(),
                    tonic::Code::NotFound
                );
                continue;
            }
            let result = f.backend.blame(f.path(n, path)).unwrap();
            let expected = git(
                &f.source,
                &[
                    "blame",
                    "--line-porcelain",
                    &f.oids[n].to_string(),
                    "--",
                    std::str::from_utf8(path).unwrap(),
                ],
                b"",
            );
            let expected: Vec<_> = expected
                .split(|b| *b == b'\n')
                .filter_map(|line| {
                    let fields: Vec<_> = line.split(|b| *b == b' ').collect();
                    if fields.len() >= 3 && fields[0].len() == 40 {
                        Some((
                            oid_from_hex(fields[0]).ok()?,
                            std::str::from_utf8(fields[1]).ok()?.parse::<u64>().ok()?,
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            let actual: Vec<_> = result
                .lines
                .iter()
                .map(|l| {
                    assert_eq!(l.path, path);
                    (f.oid(&l.changeset), l.line)
                })
                .collect();
            assert_eq!(actual, expected, "blame n={n} path={path:?}");
        }
        for b in 0..f.ids.len() {
            let result = f
                .backend
                .merge_base(p::CompareRequest {
                    repo: b"repo".to_vec(),
                    base: f.ids[n].0.to_vec(),
                    head: f.ids[b].0.to_vec(),
                })
                .unwrap();
            let mut actual: Vec<_> = result.ids.iter().map(|id| f.oid(id)).collect();
            actual.sort();
            let expected = git(
                &f.source,
                &[
                    "merge-base",
                    "--all",
                    &f.oids[n].to_string(),
                    &f.oids[b].to_string(),
                ],
                b"",
            );
            let mut expected: Vec<_> = expected
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .map(|l| oid_from_hex(l).unwrap())
                .collect();
            expected.sort();
            assert_eq!(actual, expected);
        }
    }
    for (a, b) in [(0, 3), (1, 3), (3, 4), (4, 5), (5, 6), (0, 6)] {
        let result = f
            .backend
            .diff(p::DiffRequest {
                repo: b"repo".to_vec(),
                base: f.ids[a].0.to_vec(),
                head: f.ids[b].0.to_vec(),
                paths: vec![],
                content: true,
            })
            .unwrap();
        let raw = git(
            &f.source,
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                &f.oids[a].to_string(),
                &f.oids[b].to_string(),
            ],
            b"",
        );
        let expected: Vec<_> = raw
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| p.to_vec())
            .collect();
        assert_eq!(
            result
                .files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>(),
            expected
        );
        let patch = git(
            &f.source,
            &[
                "diff",
                "--no-renames",
                "--unified=0",
                &f.oids[a].to_string(),
                &f.oids[b].to_string(),
                "--",
                "f",
            ],
            b"",
        );
        fn range(raw: &[u8]) -> (u64, usize) {
            let text = std::str::from_utf8(&raw[1..]).unwrap();
            let mut parts = text.split(',');
            let start = parts.next().unwrap().parse::<u64>().unwrap();
            let count = parts
                .next()
                .map(|s| s.parse::<usize>().unwrap())
                .unwrap_or(1);
            (if count == 0 { start } else { start - 1 }, count)
        }
        let expected: Vec<_> = patch
            .split(|b| *b == b'\n')
            .filter(|line| line.starts_with(b"@@ "))
            .map(|line| {
                let fields: Vec<_> = line.split(|b| *b == b' ').collect();
                (range(fields[1]), range(fields[2]))
            })
            .collect();
        let actual: Vec<_> = result
            .files
            .iter()
            .flat_map(|f| {
                f.edits.iter().map(|e| {
                    (
                        (e.before_start, e.before.len()),
                        (e.after_start, e.after.len()),
                    )
                })
            })
            .collect();
        assert_eq!(actual, expected, "content diff {a}..{b}");
        let old = f
            .backend
            .read_file(f.path(a, b"f"))
            .map(|r| r.content)
            .unwrap_or_default();
        let new = f
            .backend
            .read_file(f.path(b, b"f"))
            .map(|r| r.content)
            .unwrap_or_default();
        for file in result.files {
            let old_lines = lines(&old);
            let mut rebuilt = Vec::new();
            let mut offset = 0;
            for edit in file.edits {
                let pos = edit.before_start as usize;
                for line in &old_lines[offset..pos] {
                    rebuilt.extend_from_slice(line);
                }
                for line in &edit.after {
                    rebuilt.extend(line);
                }
                offset = pos + edit.before.len();
            }
            for line in &old_lines[offset..] {
                rebuilt.extend_from_slice(line);
            }
            assert_eq!(rebuilt, new);
        }
    }
}
#[tokio::test]
async fn grpc_over_tcp_and_http_json_preserve_bytes_and_enforce_access() {
    let f = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let service = MetadataService::new(f.backend.clone()).into_server();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let mut client = MetadataClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    assert_eq!(
        client.get_changeset(f.request(0)).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    fn auth<T>(q: T) -> tonic::Request<T> {
        let mut q = tonic::Request::new(q);
        q.metadata_mut()
            .insert("authorization", "Bearer secret".parse().unwrap());
        q
    }
    let native = f.backend.get_changeset(f.request(0)).unwrap();
    assert_eq!(
        client
            .get_changeset(auth(f.request(0)))
            .await
            .unwrap()
            .into_inner(),
        native
    );
    let data = client
        .read_file(auth(f.path(0, b"other")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(data.content, b"untouched\xff\n");
    let binary_path = client
        .read_file(auth(f.path(0, b"\xff")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(binary_path.content, b"byte path\n");

    assert_eq!(
        client
            .get_root_tree(auth(f.request(0)))
            .await
            .unwrap()
            .into_inner()
            .id
            .len(),
        32
    );
    let p::entry::Kind::File(file) = data.entry.as_ref().unwrap().kind.as_ref().unwrap() else {
        panic!()
    };
    let metadata_request = p::ObjectRequest {
        repo: b"repo".to_vec(),
        ids: vec![file.blob.clone()],
    };
    let metadata = client
        .get_blob_metadata(auth(metadata_request.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(metadata.blobs.len(), 1);
    assert_eq!(metadata.blobs[0].id, file.blob);
    assert_eq!(metadata.blobs[0].size, data.content.len() as u64);
    assert_eq!(
        metadata.blobs[0].git_oid,
        GitOid::hash("blob", &data.content).0
    );
    let response = http_router(f.backend.clone())
        .oneshot(
            Request::post("/v1/blob-metadata")
                .header("Authorization", "Bearer secret")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&metadata_request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<p::BlobMetadataResponse>(&body).unwrap(),
        metadata
    );
    assert_eq!(
        client
            .list_refs(auth(p::ListRefsRequest {
                repo: b"repo".to_vec(),
                prefix: b"refs/heads/".to_vec()
            }))
            .await
            .unwrap()
            .into_inner()
            .refs
            .len(),
        5
    );
    assert_eq!(
        client
            .resolve_ref(auth(p::RefRequest {
                repo: b"repo".to_vec(),
                name: b"refs/heads/main".to_vec()
            }))
            .await
            .unwrap()
            .into_inner()
            .target,
        Some(f.ids[6].0.to_vec())
    );
    assert!(
        client
            .is_ancestor(auth(p::CompareRequest {
                repo: b"repo".to_vec(),
                base: f.ids[2].0.to_vec(),
                head: f.ids[3].0.to_vec()
            }))
            .await
            .unwrap()
            .into_inner()
            .is_ancestor
    );
    assert_eq!(
        client
            .merge_base(auth(p::CompareRequest {
                repo: b"repo".to_vec(),
                base: f.ids[7].0.to_vec(),
                head: f.ids[8].0.to_vec()
            }))
            .await
            .unwrap()
            .into_inner()
            .ids
            .len(),
        2
    );
    assert!(
        !client
            .diff(auth(p::DiffRequest {
                repo: b"repo".to_vec(),
                base: f.ids[0].0.to_vec(),
                head: f.ids[3].0.to_vec(),
                paths: vec![],
                content: true
            }))
            .await
            .unwrap()
            .into_inner()
            .files
            .is_empty()
    );
    assert_eq!(
        client
            .blame(auth(f.path(3, b"f")))
            .await
            .unwrap()
            .into_inner()
            .lines
            .len(),
        5
    );
    assert_eq!(
        client
            .log(auth(p::LogRequest {
                repo: b"repo".to_vec(),
                start: f.ids[6].0.to_vec(),
                path: None,
                limit: 2,
                cursor: vec![]
            }))
            .await
            .unwrap()
            .into_inner()
            .changesets
            .len(),
        2
    );
    assert_eq!(
        client
            .list_dir(auth(f.path(0, b"")))
            .await
            .unwrap()
            .into_inner()
            .entries
            .len(),
        3
    );
    assert_eq!(
        client
            .find_changeset_by_git_oid(auth(p::GitOidRequest {
                repo: b"repo".to_vec(),
                oid: f.oids[0].0.to_vec()
            }))
            .await
            .unwrap()
            .into_inner()
            .id,
        Some(f.ids[0].0.to_vec())
    );
    let response = http_router(f.backend.clone())
        .oneshot(
            Request::post("/v1/read-file")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&f.path(0, b"other")).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json["entry"]["kind"]["file"].is_object());
    let http: p::ReadFileResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(http, data);
    let response = http_router(f.backend.clone())
        .oneshot(
            Request::post("/v1/read-file")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&f.path(0, b"\xff")).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let http: p::ReadFileResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(http, binary_path);

    let response = http_router(f.backend.clone())
        .oneshot(
            Request::post("/v1/changeset")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&f.request(0)).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let wrong = p::ChangesetRequest {
        repo: b"wrong".to_vec(),
        ..f.request(0)
    };
    assert_eq!(
        client.get_changeset(auth(wrong)).await.unwrap_err().code(),
        tonic::Code::PermissionDenied
    );
    let malformed = p::ChangesetRequest {
        repo: b"repo".to_vec(),
        id: vec![0; 31],
    };
    assert_eq!(
        client
            .get_changeset(auth(malformed))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    let _ = stop.send(());
    server.await.unwrap();
}
#[test]
fn membership_filters_orphans_from_other_repositories_and_cursors_are_bound() {
    let f = Fixture::new();
    let repo = Repository {
        meta: f.meta.as_ref(),
        blobs: f.blobs.as_ref(),
    };
    let mut cs = repo.changeset(f.ids[0]).unwrap();
    cs.message = b"other repo only".as_slice().into();
    cs.git_provenance = None;
    let id = repo.put_changeset(&cs).unwrap();
    repo.associate_changeset(b"other", id).unwrap();
    let mut child = cs.clone();
    child.parents = vec![id];
    child.message = b"child".as_slice().into();
    let child_id = repo.put_changeset(&child).unwrap();
    assert!(repo.associate_changeset(b"repo", child_id).is_err());
    repo.associate_changeset(b"other", child_id).unwrap();

    assert_eq!(
        f.backend
            .get_changeset(p::ChangesetRequest {
                repo: b"repo".to_vec(),
                id: id.0.to_vec()
            })
            .unwrap_err()
            .code(),
        tonic::Code::NotFound
    );
    let q = p::LogRequest {
        repo: b"repo".to_vec(),
        start: f.ids[6].0.to_vec(),
        path: None,
        limit: 1,
        cursor: vec![],
    };
    let page = f.backend.log(q.clone()).unwrap();
    assert!(!page.cursor.is_empty());
    let mut outside = page.cursor.clone();
    let len = outside.len();
    outside[len - 32..].copy_from_slice(&f.ids[2].0);
    assert!(
        f.backend
            .log(p::LogRequest {
                cursor: outside,
                ..q.clone()
            })
            .is_err()
    );

    assert!(
        f.backend
            .log(p::LogRequest {
                path: Some(b"other".to_vec()),
                cursor: page.cursor,
                ..q.clone()
            })
            .is_err()
    );
    assert!(
        f.backend
            .log(p::LogRequest {
                limit: 1001,
                ..q.clone()
            })
            .is_err()
    );
    assert!(f.backend.log(p::LogRequest { limit: 0, ..q }).is_err());
}

#[test]
fn lazy_history_materializes_only_the_requested_path() {
    let f = Fixture::new();
    let engine = Engine::standard();
    let ctx = Ctx {
        repo: Repository {
            meta: f.meta.as_ref(),
            blobs: f.blobs.as_ref(),
        },
    };
    let path = RepoPath::new(b"f".as_slice()).unwrap();
    let other = RepoPath::new(b"other".as_slice()).unwrap();
    engine.derive::<PathHistory>(&ctx, f.ids[3], &path).unwrap();
    engine.derive::<Blame>(&ctx, f.ids[3], &path).unwrap();
    for id in &f.ids {
        assert!(
            ctx.repo
                .meta
                .get(&DerivedRef::of::<PathHistory>(*id, &other).storage_key())
                .unwrap()
                .is_none()
        );
        assert!(
            ctx.repo
                .meta
                .get(&DerivedRef::of::<Blame>(*id, &other).storage_key())
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_binary_serves_grpc_and_http_from_persistent_store() {
    use std::io::{BufRead, BufReader};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let f = Fixture::new();
    let state_dir = f._dir.path().join("persistent");
    std::fs::create_dir_all(&state_dir).unwrap();
    let meta = SqliteStore::open(state_dir.join("meta.db")).unwrap();
    let blobs = FsBlobStore::open(state_dir.join("objects")).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: ProjectionId::full(b"repo".as_slice()),
        },
    };
    let report = importer.import(&f.source).unwrap();
    importer.publish(&meta, &report, b"test", 0).unwrap();
    let mut child = Child(
        Command::new(env!("CARGO_BIN_EXE_kasu-server"))
            .arg(&state_dir)
            .args(["repo", "127.0.0.1:0", "127.0.0.1:0"])
            .env("KASU_TOKEN", "secret")
            .env("RUST_LOG", "info")
            .env("NO_COLOR", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = BufReader::new(child.0.stderr.take().unwrap());
    let mut startup = String::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            stderr.read_line(&mut line).unwrap(),
            0,
            "server exited before startup: {startup}"
        );
        startup.push_str(&line);
        if line.contains("kasu metadata server listening") {
            break;
        }
    }
    fn address(log: &str, key: &str) -> String {
        log.split_whitespace()
            .find_map(|word| word.strip_prefix(key))
            .unwrap()
            .to_string()
    }
    let grpc = address(&startup, "grpc=");
    let http = address(&startup, "http=");
    let mut client = MetadataClient::connect(format!("http://{grpc}"))
        .await
        .unwrap();
    let mut request = tonic::Request::new(f.request(0));
    request
        .metadata_mut()
        .insert("authorization", "Bearer secret".parse().unwrap());
    assert_eq!(
        client.get_changeset(request).await.unwrap().into_inner().id,
        f.ids[0].0
    );
    let body = serde_json::to_vec(&f.path(0, b"other")).unwrap();
    let request = format!(
        "POST /v1/read-file HTTP/1.1\r\nHost: {http}\r\nAuthorization: Bearer secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut socket = tokio::net::TcpStream::connect(http).await.unwrap();
    socket.write_all(request.as_bytes()).await.unwrap();
    socket.write_all(&body).await.unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).await.unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200"));
    let offset = response.windows(4).position(|s| s == b"\r\n\r\n").unwrap() + 4;
    let data: p::ReadFileResponse = serde_json::from_slice(&response[offset..]).unwrap();
    assert_eq!(data.content, b"untouched\xff\n");
}
