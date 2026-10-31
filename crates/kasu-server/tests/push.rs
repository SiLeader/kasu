use kasu_derived::Engine;
use kasu_git::*;
use kasu_ingest::Importer;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::Arc,
};
use tower::ServiceExt;

fn command(path: &Path, args: &[&str], input: &[u8]) -> Output {
    command_traced(path, args, input, None)
}
fn command_traced(path: &Path, args: &[&str], input: &[u8], trace: Option<&Path>) -> Output {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
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
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@b")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@b")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(trace) = trace {
        cmd.env("GIT_TRACE_PACKET", trace);
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}
fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn git(path: &Path, args: &[&str]) -> Vec<u8> {
    success(command(path, args, b""))
}
fn oid(path: &Path, name: &str) -> String {
    String::from_utf8(git(path, &["rev-parse", name]))
        .unwrap()
        .trim()
        .into()
}
fn git_version() -> String {
    String::from_utf8(
        Command::new("git")
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .split_whitespace()
    .nth(2)
    .unwrap()
    .to_owned()
}
fn receive_command(old: &str, new: &str, name: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_packet(
        &mut bytes,
        format!("{old} {new} {name}\0report-status\n").as_bytes(),
    )
    .unwrap();
    bytes.extend(b"0000");
    bytes
}
fn commit(path: &Path, message: &str) -> String {
    git(path, &["add", "."]);
    git(path, &["commit", "-qm", message]);
    oid(path, "HEAD")
}

fn atomic_commands(commands: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (n, (old, new, name)) in commands.iter().enumerate() {
        let caps = if n == 0 { "\0report-status atomic" } else { "" };
        write_packet(&mut bytes, format!("{old} {new} {name}{caps}\n").as_bytes()).unwrap();
    }
    bytes.extend(b"0000");
    bytes
}

fn assert_all_rejected(report: &[u8], count: usize) {
    let packets = parse_packets(report).unwrap();
    assert!(packets.contains(&Packet::Data(b"unpack ok\n")));
    assert_eq!(
        packets
            .iter()
            .filter(|p| matches!(p, Packet::Data(line) if line.starts_with(b"ng refs/")))
            .count(),
        count
    );
    assert!(
        !packets
            .iter()
            .any(|p| matches!(p, Packet::Data(line) if line.starts_with(b"ok refs/")))
    );
}
struct Running {
    _temp: tempfile::TempDir,
    path: PathBuf,
    url: String,
    state: Arc<Backend>,
    meta: Arc<SqliteStore>,
    server: tokio::task::JoinHandle<()>,
    sequence: std::sync::atomic::AtomicUsize,
    capture: String,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Running {
    async fn new(policy: WritePolicy, seed: impl FnOnce(&Path)) -> Self {
        let version = git_version();
        let capture = if policy.unrestricted_paths() {
            format!("git-{version}-http-push.trace")
        } else {
            format!("git-{version}-http-push-acl.trace")
        };
        Self::new_labeled(policy, seed, &capture).await
    }
    async fn new_labeled(policy: WritePolicy, seed: impl FnOnce(&Path), capture: &str) -> Self {
        let capture = capture.to_owned();
        if let Some(dir) = std::env::var_os("KASU_CAPTURE_DIR").filter(|_| !capture.is_empty()) {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(Path::new(&dir).join(&capture), b"").unwrap();
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("client");
        std::fs::create_dir(&path).unwrap();
        git(&path, &["init", "--template=", "--initial-branch=main"]);
        seed(&path);
        let meta = Arc::new(SqliteStore::open(temp.path().join("meta.db")).unwrap());
        let blobs = Arc::new(FsBlobStore::open(temp.path().join("objects")).unwrap());
        if command(&path, &["rev-parse", "HEAD"], b"").status.success() {
            let bare = temp.path().join("seed.git");
            git(
                &path,
                &[
                    "clone",
                    "--bare",
                    "--no-local",
                    path.to_str().unwrap(),
                    bare.to_str().unwrap(),
                ],
            );
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
            let report = importer.import(bare).unwrap();
            importer
                .publish(meta.as_ref(), &report, b"seed", 0)
                .unwrap();
        }
        let state = Arc::new(
            Backend::new(
                b"repo".to_vec(),
                meta.clone(),
                blobs,
                meta.clone(),
                Some("secret".into()),
            )
            .with_write_policy(policy),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/repo.git",
            listener.local_addr().unwrap().port()
        );
        let router = http_router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            _temp: temp,
            path,
            url,
            state,
            meta,
            server,
            sequence: std::sync::atomic::AtomicUsize::new(0),
            capture,
        }
    }
    async fn push(&self, args: &[&str]) -> Output {
        let path = self.path.clone();
        let url = self.url.clone();
        let args: Vec<_> = args.iter().map(|s| s.to_string()).collect();
        let trace = self._temp.path().join(format!(
            "push-{}.trace",
            self.sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let capture = std::env::var_os("KASU_CAPTURE_DIR")
            .filter(|_| !self.capture.is_empty())
            .map(|dir| Path::new(&dir).join(&self.capture));
        tokio::task::spawn_blocking(move || {
            let mut words = vec![
                "-c",
                "http.extraHeader=Authorization: Bearer secret",
                "push",
                &url,
            ];
            words.extend(args.iter().map(String::as_str));
            let output = command_traced(
                &path,
                &words,
                b"",
                capture.as_ref().map(|_| trace.as_path()),
            );
            if let Some(capture) = capture {
                let mut saved = std::fs::OpenOptions::new()
                    .append(true)
                    .open(capture)
                    .unwrap();
                writeln!(saved, "# git push {}", args.join(" ")).unwrap();
                for line in std::fs::read_to_string(trace).unwrap_or_default().lines() {
                    if let Some((_, packet)) = line.split_once("packet:") {
                        writeln!(saved, "packet:{packet}").unwrap();
                    }
                }
            }
            output
        })
        .await
        .unwrap()
    }
    fn native(&self, oid: &str) -> Vec<u8> {
        self.state
            .find_changeset_by_git_oid(p::GitOidRequest {
                repo: b"repo".to_vec(),
                oid: oid_from_hex(oid.as_bytes()).unwrap().0.to_vec(),
            })
            .unwrap()
            .id
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_push_rejects_conflicting_ref_prefixes_without_breaking_clone() {
    let running = Running::new(WritePolicy::default(), |path| {
        std::fs::write(path.join("file"), b"content").unwrap();
        commit(path, "initial");
    })
    .await;
    success(running.push(&["HEAD:refs/heads/topic"]).await);
    let rejected = running.push(&["HEAD:refs/heads/topic/sub"]).await;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("ref name conflicts"));
    let rejected = running
        .push(&[
            "--atomic",
            "HEAD:refs/heads/other",
            "HEAD:refs/heads/other/sub",
        ])
        .await;
    assert!(!rejected.status.success());
    assert!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/other")
            .unwrap()
            .target
            .is_none()
    );
    let cloned = running._temp.path().join("cloned");
    success(command(
        &running.path,
        &[
            "-c",
            "http.extraHeader=Authorization: Bearer secret",
            "clone",
            &running.url,
            cloned.to_str().unwrap(),
        ],
        b"",
    ));
    assert_eq!(std::fs::read(cloned.join("file")).unwrap(), b"content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_push_initial_incremental_tags_force_delete_and_partial_rejection() {
    let running = Running::new(
        WritePolicy {
            allow_delete: true,
            force_ref_patterns: vec![b"refs/heads/automation/*".to_vec()],
            ..Default::default()
        },
        |_| {},
    )
    .await;
    let path = &running.path;
    std::fs::write(path.join("manifest.yaml"), b"version: one\n").unwrap();
    let text: String = (0..2000)
        .map(|n| format!("manifest line {n}: unchanged resource data\n"))
        .collect();
    std::fs::write(path.join("large.yaml"), &text).unwrap();
    std::fs::write(path.join("run.sh"), b"#!/bin/sh\necho yes\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("manifest.yaml", path.join("link")).unwrap();
    }
    let first = commit(path, "first");
    success(running.push(&["main"]).await);
    git(path, &["tag", "-a", "v1", "-m", "v1"]);
    git(path, &["tag", "-a", "nested", "v1", "-m", "nested"]);
    let blob = oid(path, "HEAD:manifest.yaml");
    git(path, &["tag", "raw-blob", &blob]);
    success(running.push(&["--tags"]).await);
    std::fs::write(
        path.join("large.yaml"),
        text.replace("manifest line 500:", "updated resource 500:"),
    )
    .unwrap();
    std::fs::write(path.join("manifest.yaml"), b"version: two\n").unwrap();
    let second = commit(path, "second");
    success(running.push(&["main"]).await);
    git(path, &["branch", "good"]);
    let bad = format!("{first}:refs/heads/main");
    let output = running
        .push(&["--force", &bad, "good:refs/heads/good"])
        .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("non-fast-forward"));
    let main = running.meta.resolve(b"repo", b"refs/heads/main").unwrap();
    assert_eq!(main.target.unwrap().0.to_vec(), running.native(&second));
    assert!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/good")
            .unwrap()
            .target
            .is_some()
    );
    success(running.push(&["main:refs/heads/automation/update"]).await);
    let output = success(command(
        path,
        &[
            "commit-tree",
            &oid(path, "HEAD^{tree}"),
            "-m",
            "unrelated automation",
        ],
        b"",
    ));
    let unrelated = String::from_utf8(output).unwrap().trim().to_owned();
    success(
        running
            .push(&[
                "--force",
                &format!("{unrelated}:refs/heads/automation/update"),
            ])
            .await,
    );
    success(
        running
            .push(&[":refs/heads/automation/update", ":refs/tags/raw-blob"])
            .await,
    );
    assert_eq!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/automation/update")
            .unwrap()
            .target,
        None
    );
    let private = receive_command(&"0".repeat(40), &unrelated, "refs/heads/private-probe");
    let denied = running.state.receive_pack(&private).unwrap();
    assert!(parse_packets(&denied).unwrap().iter().any(
        |p| matches!(p,Packet::Data(line) if line.starts_with(b"ng refs/heads/private-probe"))
    ));
    assert_eq!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/private-probe")
            .unwrap()
            .target,
        None
    );
    assert_eq!(
        running
            .meta
            .resolve_git(b"repo", b"refs/tags/raw-blob")
            .unwrap()
            .value,
        None
    );
    git(path, &["tag", "-f", "v1", &second]);
    assert!(
        !running
            .push(&["--force", "refs/tags/v1"])
            .await
            .status
            .success()
    );
    success(
        running
            .push(&["--atomic", "main:refs/heads/atomic-test"])
            .await,
    );
    assert_eq!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/atomic-test")
            .unwrap()
            .target,
        main.target
    );
    let dest = running._temp.path().join("mirror.git");
    let url = running.url.clone();
    let clone = dest.clone();
    tokio::task::spawn_blocking(move || {
        success(command(
            clone.parent().unwrap(),
            &[
                "-c",
                "http.extraHeader=Authorization: Bearer secret",
                "clone",
                "--mirror",
                &url,
                clone.to_str().unwrap(),
            ],
            b"",
        ));
        git(&clone, &["fsck", "--strict"]);
    })
    .await
    .unwrap();
    assert_eq!(oid(&dest, "refs/heads/main"), second);
    assert_eq!(
        oid(&dest, "refs/tags/nested"),
        oid(path, "refs/tags/nested")
    );
    let shallow = running._temp.path().join("shallow-writeback");
    let url = running.url.clone();
    let pushed = tokio::task::spawn_blocking(move || {
        success(command(
            shallow.parent().unwrap(),
            &[
                "-c",
                "http.extraHeader=Authorization: Bearer secret",
                "clone",
                "--depth=1",
                &url,
                shallow.to_str().unwrap(),
            ],
            b"",
        ));
        std::fs::write(shallow.join("image-writeback.yaml"), b"image: fixture:v2\n").unwrap();
        let head = commit(&shallow, "shallow write-back");
        success(command(
            &shallow,
            &[
                "-c",
                "http.extraHeader=Authorization: Bearer secret",
                "push",
                "origin",
                "main",
            ],
            b"",
        ));
        head
    })
    .await
    .unwrap();
    assert_eq!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/main")
            .unwrap()
            .target
            .unwrap()
            .0
            .to_vec(),
        running.native(&pushed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_connectivity_stale_refs_and_auth_never_publish_a_ref() {
    let running = Running::new_labeled(
        WritePolicy::default(),
        |path| {
            std::fs::write(path.join("f"), b"base").unwrap();
            commit(path, "root");
        },
        "",
    )
    .await;
    let before = running.meta.tail(b"repo", 0, 100).unwrap();
    let head = oid(&running.path, "HEAD");
    let raw = format!(
        "tree {}\nauthor A <a@b> 1700000000 +0000\ncommitter A <a@b> 1700000000 +0000\n\nmissing tree\n",
        "7".repeat(40)
    );
    let missing = String::from_utf8(success(command(
        &running.path,
        &["hash-object", "-w", "-t", "commit", "--stdin"],
        raw.as_bytes(),
    )))
    .unwrap()
    .trim()
    .to_owned();
    let pack = success(command(
        &running.path,
        &["pack-objects", "--stdout"],
        format!("{missing}\n").as_bytes(),
    ));
    let mut request = receive_command(&"0".repeat(40), &missing, "refs/heads/missing");
    request.extend(pack);
    let report = running.state.receive_pack(&request).unwrap();
    let packets = parse_packets(&report).unwrap();
    assert!(!packets.contains(&Packet::Data(b"unpack ok\n")));
    assert_eq!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/missing")
            .unwrap()
            .target,
        None
    );
    let stale = receive_command(&"9".repeat(40), &head, "refs/heads/main");
    let report = running.state.receive_pack(&stale).unwrap();
    assert!(
        parse_packets(&report).unwrap().iter().any(
            |p| matches!(p,Packet::Data(line) if line.starts_with(b"ng refs/heads/main stale"))
        )
    );
    let response = http_router(running.state.clone())
        .oneshot(
            axum::http::Request::post("/repo.git/git-receive-pack")
                .header("content-type", "application/x-git-receive-pack-request")
                .body(axum::body::Body::from(b"invalid pack".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(running.meta.tail(b"repo", 0, 100).unwrap(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_tree_expansion_rejects_every_ref_before_publication() {
    let running = Running::new_labeled(
        WritePolicy::default(),
        |path| {
            std::fs::write(path.join("f"), b"base").unwrap();
            commit(path, "root");
        },
        "",
    )
    .await;
    let before_refs = running.meta.list(b"repo", b"").unwrap();
    let before_git_refs = running.meta.list_git(b"repo", b"").unwrap();
    let before_log = running.meta.tail(b"repo", 0, 100).unwrap();
    let before_git_log = running.meta.tail_git(b"repo", 0, 100).unwrap();
    let head = oid(&running.path, "HEAD");
    let object = |args: &[&str], input: &[u8]| {
        String::from_utf8(success(command(&running.path, args, input)))
            .unwrap()
            .trim()
            .to_owned()
    };
    let blob = object(&["hash-object", "-w", "--stdin"], b"x");
    let mut tree = object(&["mktree"], format!("100644 blob {blob}\tf\n").as_bytes());
    let mut objects = vec![blob, tree.clone()];
    // Only 19 new objects, but 65,536 files and 131,070 directory occurrences.
    for _ in 0..16 {
        tree = object(
            &["mktree"],
            format!("040000 tree {tree}\ta\n040000 tree {tree}\tb\n").as_bytes(),
        );
        objects.push(tree.clone());
    }
    let new = object(&["commit-tree", &tree, "-p", &head], b"shared trees\n");
    objects.push(new.clone());
    let pack = success(command(
        &running.path,
        &["pack-objects", "--stdout"],
        format!("{}\n", objects.join("\n")).as_bytes(),
    ));
    assert!(pack.len() < 4096);
    let mut request = Vec::new();
    write_packet(
        &mut request,
        format!("{head} {new} refs/heads/main\0report-status\n").as_bytes(),
    )
    .unwrap();
    write_packet(
        &mut request,
        format!("{} {head} refs/heads/valid\n", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    request.extend(b"0000");
    request.extend(pack);
    let response = http_router(running.state.clone())
        .oneshot(
            axum::http::Request::post("/repo.git/git-receive-pack")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/x-git-receive-pack-request")
                .body(axum::body::Body::from(request))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let packets = parse_packets(&body).unwrap();
    assert!(packets.iter().any(|packet| matches!(
        packet,
        Packet::Data(line)
            if line.starts_with(b"unpack ")
                && line.windows(b"tree diff entry limit exceeded".len())
                    .any(|part| part == b"tree diff entry limit exceeded")
    )));
    for name in ["main", "valid"] {
        let rejected = format!("ng refs/heads/{name} unpacker error\n");
        assert!(packets.contains(&Packet::Data(rejected.as_bytes())));
    }
    assert_eq!(running.meta.list(b"repo", b"").unwrap(), before_refs);
    assert_eq!(
        running.meta.list_git(b"repo", b"").unwrap(),
        before_git_refs
    );
    assert_eq!(running.meta.tail(b"repo", 0, 100).unwrap(), before_log);
    assert_eq!(
        running.meta.tail_git(b"repo", 0, 100).unwrap(),
        before_git_log
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_tree_acl_blocks_second_parent_changes_and_native_publication() {
    let running = Running::new(
        WritePolicy {
            allowed_path_prefixes: vec![b"app".to_vec()],
            force_ref_patterns: vec![b"refs/heads/main".to_vec()],
            ..Default::default()
        },
        |path| {
            std::fs::create_dir(path.join("app")).unwrap();
            std::fs::create_dir(path.join("secret")).unwrap();
            std::fs::write(path.join("app/config"), b"one").unwrap();
            std::fs::write(path.join("secret/key"), b"original").unwrap();
            commit(path, "root");
            git(path, &["checkout", "-qb", "other"]);
            std::fs::write(path.join("secret/key"), b"unauthorized").unwrap();
            commit(path, "private change");
            git(path, &["checkout", "main"]);
        },
    )
    .await;
    let main = oid(&running.path, "main");
    let other = oid(&running.path, "other");
    let merge = success(command(
        &running.path,
        &[
            "commit-tree",
            &oid(&running.path, "other^{tree}"),
            "-p",
            &other,
            "-p",
            &main,
            "-m",
            "empty first-parent diff",
        ],
        b"",
    ));
    let merge = String::from_utf8(merge).unwrap().trim().to_owned();
    let output = running
        .push(&["--force", &format!("{merge}:refs/heads/main")])
        .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("final tree"));
    let id = running.native(&merge);
    assert!(
        running
            .state
            .get_changeset(p::ChangesetRequest {
                repo: b"repo".to_vec(),
                id: id.clone()
            })
            .unwrap()
            .changes
            .is_empty()
    );
    let current = running
        .state
        .resolve_ref(p::RefRequest {
            repo: b"repo".to_vec(),
            name: b"refs/heads/main".to_vec(),
        })
        .unwrap();
    let denied = running
        .state
        .update_ref(p::UpdateRefRequest {
            repo: b"repo".to_vec(),
            name: current.name.clone(),
            expected: Some(current.clone()),
            target: Some(id),
            force: true,
        })
        .unwrap_err();
    assert_eq!(denied.code(), tonic::Code::PermissionDenied);
    let response = running
        .state
        .clone()
        .land(p::LandRequest {
            required_ancestor: Vec::new(),
            repo: b"repo".to_vec(),
            id: running.native(&other),
            name: current.name,
            idempotency_key: b"acl-land".to_vec(),
            max_attempts: 2,
            timeout_ms: 1000,
            require_unborn: false,
        })
        .await
        .unwrap();
    assert_eq!(response.outcome, p::LandOutcome::Denied as i32);
    std::fs::write(running.path.join("app/config"), b"two").unwrap();
    let allowed = commit(&running.path, "allowed");
    success(running.push(&["main"]).await);
    assert_eq!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/main")
            .unwrap()
            .target
            .unwrap()
            .0
            .to_vec(),
        running.native(&allowed)
    );
    let read = running
        .state
        .read_file(p::PathRequest {
            follow_copies: false,
            follow_moves: false,
            repo: b"repo".to_vec(),
            id: running.native(&allowed),
            path: b"secret/key".to_vec(),
        })
        .unwrap();
    assert_eq!(read.content, b"original");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned Go client in KASU_GO_GIT_PUSH_CLIENT"]
async fn pinned_go_git_full_and_shallow_clone_commit_push() {
    let binary = std::env::var_os("KASU_GO_GIT_PUSH_CLIENT").expect("KASU_GO_GIT_PUSH_CLIENT");
    let running = Running::new_labeled(
        WritePolicy::default(),
        |path| {
            std::fs::write(path.join("manifest.yaml"), b"one\n").unwrap();
            commit(path, "first");
            std::fs::write(path.join("manifest.yaml"), b"two\n").unwrap();
            commit(path, "second");
        },
        "",
    )
    .await;
    for depth in [0, 1] {
        let destination = running._temp.path().join(format!("go-{depth}"));
        let binary = binary.clone();
        let url = running.url.clone();
        let path = destination.clone();
        let output = tokio::task::spawn_blocking(move || {
            Command::new(binary)
                .arg(url)
                .arg(path)
                .arg(depth.to_string())
                .env("KASU_TOKEN", "secret")
                .env("KASU_PUSH", "1")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        if let Some(dir) = std::env::var_os("KASU_CAPTURE_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                Path::new(&dir).join(format!("go-git-5.19.3-http-push-depth{depth}.trace")),
                &output.stderr,
            )
            .unwrap();
        }
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let oid = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        let native = running.native(&oid);
        assert_eq!(
            running
                .meta
                .resolve(b"repo", b"refs/heads/main")
                .unwrap()
                .target
                .unwrap()
                .0
                .to_vec(),
            native
        );
        let file = running
            .state
            .read_file(p::PathRequest {
                follow_copies: false,
                follow_moves: false,
                repo: b"repo".to_vec(),
                id: native,
                path: b"go-git-writeback.yaml".to_vec(),
            })
            .unwrap();
        assert_eq!(
            file.content,
            format!("client: go-git\ndepth: {depth}\n").as_bytes()
        );
        git(&destination, &["fsck", "--strict"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atomic_push_creates_updates_and_deletes_branches_and_tags_together() {
    let running = Running::new_labeled(
        WritePolicy {
            allow_delete: true,
            ..Default::default()
        },
        |path| {
            std::fs::write(path.join("f"), b"one").unwrap();
            commit(path, "one");
            git(path, &["branch", "obsolete"]);
            git(path, &["tag", "-a", "v1", "-m", "one"]);
        },
        &format!("git-{}-http-atomic-push.trace", git_version()),
    )
    .await;
    let path = &running.path;
    let first = oid(path, "HEAD");
    std::fs::write(path.join("f"), b"two").unwrap();
    let second = commit(path, "two");
    git(path, &["tag", "-a", "v2", "-m", "two"]);
    let old_log = running.meta.tail(b"repo", 0, 100).unwrap();
    let old_git_log = running.meta.tail_git(b"repo", 0, 100).unwrap();
    success(
        running
            .push(&[
                "--atomic",
                "main",
                "main:refs/heads/new",
                ":refs/heads/obsolete",
                "refs/tags/v2",
            ])
            .await,
    );
    for name in [b"refs/heads/main".as_slice(), b"refs/heads/new"] {
        assert_eq!(
            running
                .meta
                .resolve(b"repo", name)
                .unwrap()
                .target
                .unwrap()
                .0
                .to_vec(),
            running.native(&second)
        );
    }
    assert!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/obsolete")
            .unwrap()
            .target
            .is_none()
    );
    assert!(
        running
            .meta
            .resolve_git(b"repo", b"refs/tags/v2")
            .unwrap()
            .value
            .is_some()
    );
    assert_eq!(
        running.meta.tail(b"repo", 0, 100).unwrap().len(),
        old_log.len() + 3
    );
    assert_eq!(
        running.meta.tail_git(b"repo", 0, 100).unwrap().len(),
        old_git_log.len() + 1
    );

    // The client sends both commands (--force), and the server rejects tag
    // replacement. The otherwise valid new branch must not be published.
    git(path, &["tag", "-f", "v1", &second]);
    let before = running.meta.tail(b"repo", 0, 100).unwrap();
    let before_git = running.meta.tail_git(b"repo", 0, 100).unwrap();
    let output = running
        .push(&[
            "--atomic",
            "--force",
            "main:refs/heads/untouched",
            "refs/tags/v1",
        ])
        .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("tag replacement"));
    assert!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/untouched")
            .unwrap()
            .target
            .is_none()
    );
    assert_eq!(running.meta.tail(b"repo", 0, 100).unwrap(), before);
    assert_eq!(running.meta.tail_git(b"repo", 0, 100).unwrap(), before_git);

    // A stale branch in the batch also rolls back a valid tag creation.
    let stale = atomic_commands(&[
        (&first, &second, "refs/heads/main"),
        (&"0".repeat(40), &second, "refs/tags/stale-batch"),
    ]);
    assert_all_rejected(&running.state.receive_pack(&stale).unwrap(), 2);
    assert!(
        running
            .meta
            .resolve_git(b"repo", b"refs/tags/stale-batch")
            .unwrap()
            .value
            .is_none()
    );
    assert_eq!(running.meta.tail(b"repo", 0, 100).unwrap(), before);
    assert_eq!(running.meta.tail_git(b"repo", 0, 100).unwrap(), before_git);

    success(
        running
            .push(&["--atomic", ":refs/heads/new", ":refs/tags/v2"])
            .await,
    );
    assert!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/new")
            .unwrap()
            .target
            .is_none()
    );
    assert!(
        running
            .meta
            .resolve_git(b"repo", b"refs/tags/v2")
            .unwrap()
            .value
            .is_none()
    );
    let dest = running._temp.path().join("atomic-mirror.git");
    let url = running.url.clone();
    let cloned = dest.clone();
    tokio::task::spawn_blocking(move || {
        success(command(
            cloned.parent().unwrap(),
            &[
                "-c",
                "http.extraHeader=Authorization: Bearer secret",
                "clone",
                "--mirror",
                &url,
                cloned.to_str().unwrap(),
            ],
            b"",
        ));
        git(&cloned, &["fsck", "--strict"]);
    })
    .await
    .unwrap();
    assert_eq!(oid(&dest, "refs/heads/main"), second);
    assert_eq!(oid(&dest, "refs/tags/v1^{}"), first);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atomic_push_still_applies_final_tree_acl_to_every_ref() {
    let running = Running::new_labeled(
        WritePolicy {
            allowed_path_prefixes: vec![b"app".to_vec()],
            ..Default::default()
        },
        |path| {
            std::fs::create_dir(path.join("app")).unwrap();
            std::fs::write(path.join("app/config"), b"allowed").unwrap();
            commit(path, "root");
        },
        "",
    )
    .await;
    let before = running.meta.tail(b"repo", 0, 100).unwrap();
    let before_git = running.meta.tail_git(b"repo", 0, 100).unwrap();
    std::fs::write(running.path.join("secret"), b"not allowed").unwrap();
    commit(&running.path, "secret");
    git(&running.path, &["tag", "-a", "denied", "-m", "denied"]);
    let output = running
        .push(&["--atomic", "HEAD~1:refs/heads/good", "refs/tags/denied"])
        .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("final tree"));
    assert!(
        running
            .meta
            .resolve(b"repo", b"refs/heads/good")
            .unwrap()
            .target
            .is_none()
    );
    assert!(
        running
            .meta
            .resolve_git(b"repo", b"refs/tags/denied")
            .unwrap()
            .value
            .is_none()
    );
    assert_eq!(running.meta.tail(b"repo", 0, 100).unwrap(), before);
    assert_eq!(running.meta.tail_git(b"repo", 0, 100).unwrap(), before_git);
}

// Force a second connection to change a ref after all command validation and
// before the production mixed CAS. Returning to the same OID tests ABA too.
struct ControlledRefs {
    store: Arc<SqliteStore>,
    other: SqliteStore,
    enabled: bool,
    race: std::sync::atomic::AtomicU8,
}
impl RefStore for ControlledRefs {
    fn capabilities(&self) -> RefCapabilities {
        self.store.capabilities()
    }
    fn resolve(&self, repo: &[u8], name: &[u8]) -> kasu_core::Result<RefState> {
        self.store.resolve(repo, name)
    }
    fn list(&self, repo: &[u8], prefix: &[u8]) -> kasu_core::Result<Vec<(Vec<u8>, RefState)>> {
        self.store.list(repo, prefix)
    }
    fn compare_and_swap(
        &self,
        repo: &[u8],
        updates: &[RefUpdate],
        actor: &[u8],
        time: i64,
    ) -> kasu_core::Result<CasResult> {
        self.store.compare_and_swap(repo, updates, actor, time)
    }
    fn tail(&self, repo: &[u8], after: u64, limit: usize) -> kasu_core::Result<Vec<RefLog>> {
        self.store.tail(repo, after, limit)
    }
}
impl GitRefStore for ControlledRefs {
    fn supports_atomic_publication(&self) -> bool {
        self.enabled
    }
    fn resolve_git(&self, repo: &[u8], name: &[u8]) -> kasu_core::Result<GitRefState> {
        self.store.resolve_git(repo, name)
    }
    fn list_git(
        &self,
        repo: &[u8],
        prefix: &[u8],
    ) -> kasu_core::Result<Vec<(Vec<u8>, GitRefState)>> {
        self.store.list_git(repo, prefix)
    }
    fn compare_git(
        &self,
        repo: &[u8],
        updates: &[GitRefUpdate],
        actor: &[u8],
        time: i64,
    ) -> kasu_core::Result<GitCasResult> {
        self.store.compare_git(repo, updates, actor, time)
    }
    fn tail_git(&self, repo: &[u8], after: u64, limit: usize) -> kasu_core::Result<Vec<GitRefLog>> {
        self.store.tail_git(repo, after, limit)
    }
    fn compare_native_and_git(
        &self,
        repo: &[u8],
        native: &[RefUpdate],
        git: &[GitRefUpdate],
        actor: &[u8],
        time: i64,
    ) -> kasu_core::Result<RefPublicationResult> {
        assert!(
            self.enabled,
            "unsupported backend must not attempt an atomic transaction"
        );
        match self.race.swap(0, std::sync::atomic::Ordering::SeqCst) {
            1 => {
                let update = &native[0];
                assert!(matches!(
                    self.other.compare_and_swap(
                        repo,
                        &[RefUpdate {
                            name: update.name.clone(),
                            expected: update.expected,
                            new: None
                        }],
                        b"racer",
                        time
                    )?,
                    CasResult::Applied(_)
                ));
                let expected = self.other.resolve(repo, &update.name)?;
                assert!(matches!(
                    self.other.compare_and_swap(
                        repo,
                        &[RefUpdate {
                            name: update.name.clone(),
                            expected,
                            new: update.expected.target
                        }],
                        b"racer",
                        time
                    )?,
                    CasResult::Applied(_)
                ));
            }
            2 => {
                let update = &git[0];
                assert!(matches!(
                    self.other.compare_git(
                        repo,
                        &[GitRefUpdate {
                            name: update.name.clone(),
                            expected: update.expected.clone(),
                            new: None
                        }],
                        b"racer",
                        time
                    )?,
                    GitCasResult::Applied(_)
                ));
                let expected = self.other.resolve_git(repo, &update.name)?;
                assert!(matches!(
                    self.other.compare_git(
                        repo,
                        &[GitRefUpdate {
                            name: update.name.clone(),
                            expected,
                            new: update.expected.value.clone()
                        }],
                        b"racer",
                        time
                    )?,
                    GitCasResult::Applied(_)
                ));
            }
            _ => {}
        }
        self.store
            .compare_native_and_git(repo, native, git, actor, time)
    }
}
impl Running {
    fn controlled(&self, enabled: bool, race: u8) -> Backend {
        Backend::new(
            b"repo".to_vec(),
            self.meta.clone(),
            Arc::new(FsBlobStore::open(self._temp.path().join("objects")).unwrap()),
            Arc::new(ControlledRefs {
                store: self.meta.clone(),
                other: SqliteStore::open(self._temp.path().join("meta.db")).unwrap(),
                enabled,
                race: std::sync::atomic::AtomicU8::new(race),
            }),
            Some("secret".into()),
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atomic_push_detects_branch_and_tag_aba_between_validation_and_commit() {
    for race in [1, 2] {
        let running = Running::new_labeled(
            WritePolicy::default(),
            |path| {
                std::fs::write(path.join("f"), b"root").unwrap();
                commit(path, "root");
                git(path, &["tag", "v1"]);
            },
            "",
        )
        .await;
        let backend = running.controlled(true, race);
        let native = running.meta.resolve(b"repo", b"refs/heads/main").unwrap();
        let tag = running.meta.resolve_git(b"repo", b"refs/tags/v1").unwrap();
        let before = running.meta.tail(b"repo", 0, 100).unwrap();
        let before_git = running.meta.tail_git(b"repo", 0, 100).unwrap();
        let head = oid(&running.path, "HEAD");
        let request = atomic_commands(&[
            (&head, &head, "refs/heads/main"),
            (&head, &head, "refs/tags/v1"),
        ]);
        let response = backend.receive_pack(&request).unwrap();
        assert_all_rejected(&response, 2);
        assert!(String::from_utf8_lossy(&response).contains("concurrent update"));
        let now = running.meta.resolve(b"repo", b"refs/heads/main").unwrap();
        let now_tag = running.meta.resolve_git(b"repo", b"refs/tags/v1").unwrap();
        assert_eq!(now.target, native.target);
        assert_eq!(now_tag.value, tag.value);
        assert_eq!(now.version, native.version + if race == 1 { 2 } else { 0 });
        assert_eq!(now_tag.version, tag.version + if race == 2 { 2 } else { 0 });
        let after = running.meta.tail(b"repo", 0, 100).unwrap();
        let after_git = running.meta.tail_git(b"repo", 0, 100).unwrap();
        assert_eq!(after.len(), before.len() + if race == 1 { 2 } else { 0 });
        assert_eq!(
            after_git.len(),
            before_git.len() + if race == 2 { 2 } else { 0 }
        );
        assert!(
            after[before.len()..]
                .iter()
                .all(|entry| entry.actor == b"racer")
        );
        assert!(
            after_git[before_git.len()..]
                .iter()
                .all(|entry| entry.actor == b"racer")
        );
        // Retry with a fresh server-observed version can succeed after the race.
        let response = backend.receive_pack(&request).unwrap();
        assert!(
            !parse_packets(&response)
                .unwrap()
                .iter()
                .any(|p| matches!(p, Packet::Data(line) if line.starts_with(b"ng ")))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atomic_capability_depends_on_the_backend_and_unadvertised_requests_are_rejected() {
    let running = Running::new_labeled(
        WritePolicy::default(),
        |path| {
            std::fs::write(path.join("f"), b"root").unwrap();
            commit(path, "root");
        },
        "",
    )
    .await;
    for enabled in [false, true] {
        let backend = Arc::new(running.controlled(enabled, 0));
        let response = http_router(backend.clone())
            .oneshot(
                axum::http::Request::get("/repo.git/info/refs?service=git-receive-pack")
                    .header("authorization", "Bearer secret")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&body).contains(" atomic"), enabled);
        if !enabled {
            let before = running.meta.tail(b"repo", 0, 100).unwrap();
            let head = oid(&running.path, "HEAD");
            let request = atomic_commands(&[(&"0".repeat(40), &head, "refs/heads/no-atomic")]);
            let error = backend.receive_pack(&request).unwrap_err();
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert!(
                running
                    .meta
                    .resolve(b"repo", b"refs/heads/no-atomic")
                    .unwrap()
                    .target
                    .is_none()
            );
            assert_eq!(running.meta.tail(b"repo", 0, 100).unwrap(), before);
            let request = receive_command(&"0".repeat(40), &head, "refs/heads/individual");
            let response = backend.receive_pack(&request).unwrap();
            assert!(
                parse_packets(&response)
                    .unwrap()
                    .contains(&Packet::Data(b"ok refs/heads/individual\n"))
            );
        }
    }
}
