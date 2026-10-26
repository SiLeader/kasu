mod common;
use axum::{Router, body::Bytes, extract::State, http::StatusCode, routing::post};
use common::*;
use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[test]
fn repositories_file_creates_updates_and_never_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repositories.json");
    std::fs::write(
        &path,
        br#"{"repositories":[
            {"name":"team-a/infra","default_branch":"trunk",
             "write_policy":{"allowed_ref_patterns":["refs/heads/release/*"]},
             "webhooks":[{"id":"argocd","url":"http://127.0.0.1:1/hook","secret_file":"secrets/argocd"}],
             "limits":{"active":4}},
            {"name":"team-b/app"}
        ]}"#,
    )
    .unwrap();
    let store = SqliteStore::memory().unwrap();
    let declared = load_repositories_file(&path).unwrap();
    // Relative secret paths follow the file, as in the other configuration files.
    assert_eq!(
        declared[0].config.webhooks[0].secret_file.as_deref(),
        Some(dir.path().join("secrets/argocd").as_path())
    );
    let report = sync_repositories(&store, &declared, ConfigSource::File, 10).unwrap();
    assert_eq!((report.created, report.updated), (2, 0));
    let record = |n: &str| {
        let key = store
            .resolve_repository_name(&name(n), 10)
            .unwrap()
            .unwrap()
            .key;
        store.repository(&key).unwrap().unwrap()
    };
    let infra = record("team-a/infra");
    assert!(!infra.key.is_legacy());
    assert_eq!(infra.default_branch, b"trunk");
    assert_eq!(infra.config_version, 1);
    let config = RepositoryConfig::parse(&infra.config).unwrap();
    assert_eq!(config.limits.active, Some(4));
    assert!(record("team-b/app").config.is_empty());
    let again = sync_repositories(&store, &declared, ConfigSource::File, 11).unwrap();
    assert_eq!((again.created, again.updated, again.unchanged), (0, 0, 2));

    // A changed declaration bumps config_version; with the registry as the
    // source of truth, the file only creates missing repositories.
    std::fs::write(
        &path,
        br#"{"repositories":[{"name":"team-a/infra"},{"name":"team-b/app"},{"name":"team-c/new"}]}"#,
    )
    .unwrap();
    let declared = load_repositories_file(&path).unwrap();
    let registry = sync_repositories(&store, &declared, ConfigSource::Registry, 12).unwrap();
    assert_eq!((registry.created, registry.updated), (1, 0));
    assert_eq!(record("team-a/infra").default_branch, b"trunk");
    let file = sync_repositories(&store, &declared, ConfigSource::File, 13).unwrap();
    assert_eq!(file.updated, 1);
    let infra = record("team-a/infra");
    assert_eq!(infra.config_version, 2);
    assert_eq!(infra.default_branch, b"main");
    assert!(infra.config.is_empty());

    // Undeclared repositories are reported, never removed.
    std::fs::write(&path, br#"{"repositories":[{"name":"team-b/app"}]}"#).unwrap();
    let declared = load_repositories_file(&path).unwrap();
    let report = sync_repositories(&store, &declared, ConfigSource::File, 14).unwrap();
    assert_eq!(report.undeclared, 2);
    assert!(
        store
            .resolve_repository_name(&name("team-a/infra"), 14)
            .unwrap()
            .is_some()
    );
    for invalid in [
        br#"{"repositories":[{"name":"a.git"}]}"#.as_slice(),
        br#"{"repositories":[{"name":"a"},{"name":"A"}]}"#,
        br#"{"repositories":[{"name":"a","default_branch":"bad..name"}]}"#,
        br#"{"repositories":[{"name":"a","limits":{"active":0}}]}"#,
        br#"{"repositories":[{"name":"a","webhooks":[{"id":"x","url":"ftp://h/"}]}]}"#,
        br#"{"repositories":[{"name":"a","typo":1}]}"#,
    ] {
        std::fs::write(&path, invalid).unwrap();
        assert!(load_repositories_file(&path).is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_write_policy_narrows_the_server_policy() {
    let f = Fixture::new(&["team-a/infra", "team-b/app"]);
    f.update("team-a/infra", |r| {
        r.config = RepositoryConfig {
            write_policy: Some(WritePolicy {
                allowed_ref_patterns: vec![b"refs/heads/release/*".to_vec()],
                ..Default::default()
            }),
            ..Default::default()
        }
        .encode();
        r.config_version += 1;
    });
    std::thread::sleep(Duration::from_millis(5));
    let first = f
        .commit("operator", "team-b/app", None, &[(b"file", b"1")])
        .await;
    assert!(!first.is_empty());
    // Even the operator cannot widen a repository's policy.
    let (status, body) = f
        .call(
            Some("operator"),
            "/v1/update-ref",
            &p::UpdateRefRequest {
                repo: b"team-a/infra".to_vec(),
                name: b"refs/heads/main".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/main".to_vec(),
                    target: None,
                    version: 0,
                }),
                target: None,
                force: false,
            },
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[derive(Default)]
struct Sink {
    bodies: Mutex<Vec<serde_json::Value>>,
}
async fn sink(State(sink): State<Arc<Sink>>, body: Bytes) -> StatusCode {
    sink.bodies
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap());
    StatusCode::OK
}
async fn wait_for(sink: &Sink, count: usize) -> Vec<serde_json::Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let bodies = sink.bodies.lock().unwrap().clone();
        if bodies.len() >= count {
            return bodies;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "missing deliveries: {bodies:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_repository_delivers_webhooks_with_its_current_name() {
    let sinks: Vec<Arc<Sink>> = (0..2).map(|_| Arc::new(Sink::default())).collect();
    let mut urls = Vec::new();
    for sink_state in &sinks {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://{}/hook", listener.local_addr().unwrap()));
        let app = Router::new()
            .route("/hook", post(sink))
            .with_state(sink_state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    }
    let f = Fixture::new(&["team-a/infra", "team-b/app", "team-c/quiet"]);
    for (repository, url) in ["team-a/infra", "team-b/app"].iter().zip(&urls) {
        f.update(repository, |r| {
            r.config = RepositoryConfig {
                webhooks: vec![WebhookEndpoint {
                    id: "gitops".into(),
                    url: url.clone(),
                    secret_file: None,
                }],
                ..Default::default()
            }
            .encode();
            r.config_version += 1;
        });
    }
    f.commit("operator", "team-a/infra", None, &[(b"a", b"1")])
        .await;
    f.commit("operator", "team-b/app", None, &[(b"b", b"1")])
        .await;
    f.commit("operator", "team-c/quiet", None, &[(b"c", b"1")])
        .await;
    let settings = WebhookConfig {
        public_url: "https://git.example.com/".into(),
        poll_interval_ms: 10,
        ..Default::default()
    };
    let supervisor = Arc::new(
        WebhookSupervisor::new(f.state.clone(), f.store.clone(), settings)
            .unwrap()
            .with_interval(Duration::from_millis(20)),
    );
    let (shutdown, stop) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(supervisor.run(stop));
    let a = wait_for(&sinks[0], 1).await;
    let b = wait_for(&sinks[1], 1).await;
    assert_eq!(a[0]["repository"]["full_name"], "team-a/infra");
    assert_eq!(a[0]["repository"]["name"], "infra");
    assert_eq!(
        a[0]["repository"]["clone_url"],
        "https://git.example.com/team-a/infra.git"
    );
    assert_eq!(b[0]["repository"]["full_name"], "team-b/app");
    // A rename restarts the dispatcher with the current name; the cursor is
    // per key, so nothing is redelivered.
    let record = f.record("team-a/infra");
    f.store
        .rename_repository(
            &record.key,
            record.version,
            &name("platform/infra"),
            None,
            b"test",
            5,
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let head = {
        let (_, body) = f
            .call(
                Some("operator"),
                "/v1/resolve-ref",
                &p::RefRequest {
                    repo: b"platform/infra".to_vec(),
                    name: b"refs/heads/main".to_vec(),
                },
            )
            .await;
        bytes(&body["target"])
    };
    f.commit("operator", "platform/infra", Some(head), &[(b"a", b"2")])
        .await;
    let a = wait_for(&sinks[0], 2).await;
    assert_eq!(a.len(), 2);
    assert_eq!(a[1]["repository"]["full_name"], "platform/infra");
    assert_ne!(a[1]["kasu"]["sequence"], a[0]["kasu"]["sequence"]);
    // Archived repositories stop delivering.
    f.update("team-b/app", |r| r.state = RepositoryState::Archived);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = shutdown.send(true);
    task.await.unwrap();
    assert_eq!(sinks[1].bodies.lock().unwrap().len(), 1);
}
