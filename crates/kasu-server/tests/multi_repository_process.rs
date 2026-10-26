use axum::{Router, body::Bytes, extract::State, http::HeaderMap, routing::post};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@b")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@b")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
/// What `kasu-ingest [--create]` does, in process (its CLI is tested in kasu-ingest).
fn ingest(source: &Path, data: &Path, name: &str, create: bool) {
    use kasu_store::RepositoryStore;
    std::fs::create_dir_all(data).unwrap();
    let meta = kasu_store::SqliteStore::open(data.join("meta.db")).unwrap();
    let blobs = kasu_store::FsBlobStore::open(data.join("objects")).unwrap();
    let name = kasu_core::RepoName::new(name).unwrap();
    let key = match meta.resolve_repository_name(&name, 0).unwrap() {
        Some(entry) => entry.key,
        None => {
            assert!(create);
            let record = kasu_store::RepositoryRecord::new(
                kasu_core::RepoKey::generate().unwrap(),
                name,
                b"test",
                0,
            );
            meta.create_repository(&record).unwrap();
            record.key
        }
    };
    let mut engine = kasu_derived::Engine::standard();
    kasu_git::register_git(&mut engine);
    let importer = kasu_ingest::Importer {
        engine: &engine,
        index: kasu_git::GitIndex {
            repo: kasu_store::Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: kasu_git::ProjectionId::full(key.as_bytes()),
        },
    };
    let report = importer.import(source).unwrap();
    importer.publish(&meta, &report, b"test", 1).unwrap();
}
#[derive(Default)]
struct Sink {
    bodies: Mutex<Vec<(String, serde_json::Value)>>,
}
async fn hook(
    State(sink): State<Arc<Sink>>,
    axum::extract::Path(slug): axum::extract::Path<String>,
    headers: HeaderMap,
    body: Bytes,
) {
    assert_eq!(
        headers["X-Hub-Signature-256"].to_str().unwrap(),
        kasu_server::webhook_signature(b"hook-secret", &body)
    );
    sink.bodies
        .lock()
        .unwrap()
        .push((slug, serde_json::from_slice(&body).unwrap()));
}
async fn deliveries(sink: &Sink, slug: &str, count: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let bodies: Vec<_> = sink
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| s == slug)
            .map(|(_, body)| body.clone())
            .collect();
        if bodies.len() >= count {
            return bodies;
        }
        assert!(Instant::now() < deadline, "{slug}: {bodies:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Real binaries: repositories created by kasu-ingest --create, declared in
/// KASU_REPOSITORIES_FILE with their own webhooks, served by a server started
/// without a repository argument, cloned by Git and updated independently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_server_process_serves_and_notifies_each_repository() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let data = root.join("data");
    let mut sources = Vec::new();
    for slug in ["a", "b"] {
        let source = root.join(format!("source-{slug}"));
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        std::fs::write(source.join("manifest"), format!("{slug} one\n")).unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "one"]);
        sources.push(source);
    }
    ingest(&sources[0], &data, "team-a/apps", true);
    ingest(&sources[1], &data, "team-b/apps", true);

    let sink = Arc::new(Sink::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hooks = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/hook/{slug}", post(hook))
        .with_state(sink.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(root.join("hook.secret"), b"hook-secret").unwrap();
    let declarations = root.join("repositories.json");
    std::fs::write(
        &declarations,
        serde_json::to_vec(&serde_json::json!({"repositories": [
            {"name": "team-a/apps", "webhooks": [{"id": "gitops", "url": format!("{hooks}/hook/a"), "secret_file": "hook.secret"}]},
            {"name": "team-b/apps", "webhooks": [{"id": "gitops", "url": format!("{hooks}/hook/b"), "secret_file": "hook.secret"}]},
        ]}))
        .unwrap(),
    )
    .unwrap();
    let log = root.join("server.log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_kasu-server"));
    command
        .args([
            data.as_os_str(),
            "127.0.0.1:0".as_ref(),
            "127.0.0.1:0".as_ref(),
        ])
        .env("KASU_TOKEN", "operator-token")
        .env("KASU_REPOSITORIES_FILE", &declarations)
        .env("KASU_PUBLIC_URL", "https://git.example.com")
        .env("RUST_LOG", "info");
    for name in [
        "KASU_POSTGRES_DSN_FILE",
        "KASU_S3_BUCKET",
        "KASU_OBJECT_TOKEN_FILE",
        "KASU_WRITE_POLICY_FILE",
        "KASU_WEBHOOKS_FILE",
        "KASU_SERVER_LIMITS_FILE",
        "KASU_ACCOUNTS_FILE",
        "KASU_PROJECTION_ACCOUNTS_FILE",
    ] {
        command.env_remove(name);
    }
    let mut server = Server(
        command
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let address = loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
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
        assert!(Instant::now() < deadline, "server did not listen: {text}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // Initial publications are delivered with each repository's own identity.
    for (slug, name) in [("a", "team-a/apps"), ("b", "team-b/apps")] {
        let body = &deliveries(&sink, slug, 1).await[0];
        assert_eq!(body["repository"]["full_name"], name);
        assert_eq!(
            body["repository"]["clone_url"],
            format!("https://git.example.com/{name}.git")
        );
    }
    // Git clients clone each repository by its multi-segment name.
    for (slug, name) in [("a", "team-a/apps"), ("b", "team-b/apps")] {
        let clone = root.join(format!("clone-{slug}"));
        git(
            root,
            &[
                "-c",
                "credential.helper=",
                "clone",
                "-q",
                &format!("http://kasu:operator-token@{address}/{name}.git"),
                clone.to_str().unwrap(),
            ],
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("manifest")).unwrap(),
            format!("{slug} one\n")
        );
    }
    // Updating one repository notifies only that repository's endpoints.
    std::fs::write(sources[0].join("manifest"), "a two\n").unwrap();
    git(&sources[0], &["commit", "-qam", "two"]);
    ingest(&sources[0], &data, "team-a/apps", false);
    let a = deliveries(&sink, "a", 2).await;
    assert_eq!(a[1]["after"], git(&sources[0], &["rev-parse", "HEAD"]));
    assert_eq!(a[1]["repository"]["full_name"], "team-a/apps");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(deliveries(&sink, "b", 1).await.len(), 1);
    // The Admin API lists the registry.
    let listed: serde_json::Value = serde_json::from_slice(
        &reqwest::Client::new()
            .post(format!("http://{address}/v1/admin/list-repositories"))
            .bearer_auth("operator-token")
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    let names: Vec<_> = listed["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["team-a/apps", "team-b/apps"]);
}
