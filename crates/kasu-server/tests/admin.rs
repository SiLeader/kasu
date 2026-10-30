mod common;
use axum::http::StatusCode;
use common::*;
use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;

fn create(name: &str, key: &[u8]) -> p::CreateRepositoryRequest {
    p::CreateRepositoryRequest {
        name: name.into(),
        default_branch: vec![],
        config_json: String::new(),
        idempotency_key: key.to_vec(),
    }
}
fn get(name: &str) -> p::GetRepositoryRequest {
    p::GetRepositoryRequest { name: name.into() }
}
fn main_line(advert: &[u8]) -> String {
    kasu_git::parse_packets(advert)
        .unwrap()
        .into_iter()
        .find_map(|packet| match packet {
            kasu_git::Packet::Data(line) => {
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                let (oid, name) = line.split_at_checked(40)?;
                (name.split(|b| *b == 0).next() == Some(b" refs/heads/main"))
                    .then(|| String::from_utf8(oid.to_vec()).unwrap())
            }
            _ => None,
        })
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_create_rename_delete_purge_and_recreate() {
    let f = Fixture::new(&[]);
    let (status, created) = f
        .call(
            Some("operator"),
            "/v1/admin/create-repository",
            &create("team-a/infra", b"create-1"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["replayed"], false);
    assert_eq!(created["state"], p::RepositoryState::Active as i32);
    assert_eq!(bytes(&created["default_branch"]), b"main");
    let key = bytes(&created["key"]);
    assert!(key.starts_with(RepoKey::PREFIX));
    // The same idempotency key replays; a different request conflicts.
    let (status, replay) = f
        .call(
            Some("operator"),
            "/v1/admin/create-repository",
            &create("team-a/infra", b"create-1"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["replayed"], true);
    assert_eq!(bytes(&replay["key"]), key);
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/admin/create-repository",
            &create("Team-A/Infra", b"other"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/admin/create-repository",
            &create("metrics", b""),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let first = f
        .commit("operator", "team-a/infra", None, &[(b"deploy/a", b"one")])
        .await;
    let (_, before) = f
        .get(
            Some("operator"),
            "/team-a/infra.git/info/refs?service=git-upload-pack",
        )
        .await;
    // Rename keeps the key and serves the old URL through an alias.
    let (status, renamed) = f
        .call(
            Some("operator"),
            "/v1/admin/rename-repository",
            &p::RenameRepositoryRequest {
                name: "team-a/infra".into(),
                new_name: "platform/infra".into(),
                keep_alias: true,
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(bytes(&renamed["key"]), key);
    assert_eq!(renamed["former_names"][0], "team-a/infra");
    for url in [
        "/platform/infra.git/info/refs?service=git-upload-pack",
        "/team-a/infra.git/info/refs?service=git-upload-pack",
    ] {
        assert_eq!(f.get(Some("operator"), url).await.1, before);
    }
    // The alias blocks reuse of the old name until it expires.
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/admin/create-repository",
            &create("team-a/infra", b""),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Configuration updates are compare-and-swap on config_version.
    let update = |version: u64| p::UpdateRepositoryRequest {
        name: "platform/infra".into(),
        expected_config_version: version,
        config_json: Some(r#"{"limits":{"active":2}}"#.into()),
        state: None,
        default_branch: Some(b"trunk".to_vec()),
    };
    let (status, updated) = f
        .call(Some("operator"), "/v1/admin/update-repository", &update(1))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["config_version"], 2);
    assert_eq!(bytes(&updated["default_branch"]), b"trunk");
    let (status, _) = f
        .call(Some("operator"), "/v1/admin/update-repository", &update(1))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/admin/update-repository",
            &p::UpdateRepositoryRequest {
                name: "platform/infra".into(),
                expected_config_version: 2,
                config_json: Some(r#"{"typo":true}"#.into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Deleting stops all access and releases the name and its aliases.
    let (status, deleted) = f
        .call(
            Some("operator"),
            "/v1/admin/delete-repository",
            &p::DeleteRepositoryRequest {
                name: "platform/infra".into(),
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["state"], p::RepositoryState::Deleting as i32);
    for url in [
        "/platform/infra.git/info/refs?service=git-upload-pack",
        "/team-a/infra.git/info/refs?service=git-upload-pack",
    ] {
        assert_eq!(f.get(Some("operator"), url).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        f.call(
            Some("operator"),
            "/v1/admin/get-repository",
            &get("platform/infra")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Purging removes the refs and every index keyed by the repository.
    let key = RepoKey::from_bytes(key).unwrap();
    let repo = Repository {
        meta: f.store.as_ref(),
        blobs: f.blobs.as_ref(),
    };
    let blob = BlobId(ObjectId::hash(b"one").0);
    assert!(repo.has_blob_membership(key.as_bytes(), blob).unwrap());
    let index = kasu_git::GitIndex {
        repo: Repository {
            meta: f.store.as_ref(),
            blobs: f.blobs.as_ref(),
        },
        projection: kasu_git::ProjectionId::full(key.as_bytes()),
    };
    let blob_oid = repo.blob_metadata(blob).unwrap().git_blob_oid;
    assert!(index.find(blob_oid).unwrap().is_some());
    assert_eq!(f.state.purge_deleted_repositories(i64::MAX - 1).unwrap(), 1);
    assert!(!repo.has_blob_membership(key.as_bytes(), blob).unwrap());
    assert!(
        repo.require_membership(
            key.as_bytes(),
            ChangesetId(first.clone().try_into().unwrap())
        )
        .is_err()
    );
    assert!(index.find(blob_oid).unwrap().is_none());
    assert_eq!(
        f.store.resolve(key.as_bytes(), b"refs/heads/main").unwrap(),
        RefState::default()
    );
    let record = f.store.repository(&key).unwrap().unwrap();
    assert!(record.purged && record.logs_purged);
    assert!(f.store.tail(key.as_bytes(), 0, 10).unwrap().is_empty());
    // The shared immutable object itself is kept for other repositories.
    assert!(repo.blob(blob).is_ok());
    assert_eq!(f.state.purge_deleted_repositories(i64::MAX - 1).unwrap(), 0);
    // Recreating the name yields a new key with nothing of the old history.
    let (_, again) = f
        .call(
            Some("operator"),
            "/v1/admin/create-repository",
            &create("platform/infra", b""),
        )
        .await;
    assert_ne!(bytes(&again["key"]), key.as_bytes());
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/changeset",
            &p::ChangesetRequest {
                repo: b"platform/infra".to_vec(),
                id: first,
            },
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_logs_of_deleted_repositories_are_kept_for_the_retention_period() {
    let f = Fixture::with(&["audit/me"], |b| {
        b.with_lifecycle(Lifecycle {
            ref_log_retention: std::time::Duration::from_secs(1000),
            ..Default::default()
        })
    });
    f.commit("operator", "audit/me", None, &[(b"a", b"1")])
        .await;
    let key = f.record("audit/me").key;
    f.call(
        Some("operator"),
        "/v1/admin/delete-repository",
        &p::DeleteRepositoryRequest {
            name: "audit/me".into(),
        },
    )
    .await;
    let deleted = f.store.repository(&key).unwrap().unwrap().deleted_at;
    assert_eq!(f.state.purge_deleted_repositories(deleted).unwrap(), 1);
    let record = f.store.repository(&key).unwrap().unwrap();
    assert!(record.purged && !record.logs_purged);
    assert_eq!(f.store.tail(key.as_bytes(), 0, 10).unwrap().len(), 1);
    f.state.purge_deleted_repositories(deleted + 1000).unwrap();
    assert!(f.store.tail(key.as_bytes(), 0, 10).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accounts_see_only_granted_repositories_and_cannot_administer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bot.pat"), "bot-pat").unwrap();
    let config = dir.path().join("accounts.json");
    std::fs::write(
        &config,
        br#"{"accounts":[{"name":"bot","token_file":"bot.pat","grants":[{"repositories":["team-a/**"]}]}]}"#,
    )
    .unwrap();
    let accounts = load_repository_accounts(&config).unwrap();
    let f = Fixture::with(&["team-a/x", "team-a/y/z", "team-b/x"], |b| {
        b.with_repository_accounts(accounts).unwrap()
    });
    let (status, listed) = f
        .call(
            Some("bot-pat"),
            "/v1/admin/list-repositories",
            &p::ListRepositoriesRequest::default(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = listed["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["team-a/x", "team-a/y/z"]);
    // Configuration is operator-only, and invisible repositories look absent.
    assert_eq!(listed["repositories"][0]["config_json"], "");
    assert_eq!(
        f.call(
            Some("bot-pat"),
            "/v1/admin/get-repository",
            &get("team-b/x")
        )
        .await,
        f.call(
            Some("bot-pat"),
            "/v1/admin/get-repository",
            &get("team-b/none")
        )
        .await
    );
    for (path, body) in [
        (
            "/v1/admin/create-repository",
            serde_json::to_value(create("team-a/new", b"")).unwrap(),
        ),
        (
            "/v1/admin/delete-repository",
            serde_json::json!({"name": "team-a/x"}),
        ),
    ] {
        assert_eq!(
            f.call(Some("bot-pat"), path, &body).await.0,
            StatusCode::FORBIDDEN
        );
    }
    // Operators page through every repository with a prefix.
    let (_, page) = f
        .call(
            Some("operator"),
            "/v1/admin/list-repositories",
            &p::ListRepositoriesRequest {
                prefix: "TEAM-".into(),
                cursor: String::new(),
                limit: 2,
            },
        )
        .await;
    assert_eq!(page["repositories"].as_array().unwrap().len(), 2);
    let (_, rest) = f
        .call(
            Some("operator"),
            "/v1/admin/list-repositories",
            &p::ListRepositoriesRequest {
                prefix: "team-".into(),
                cursor: page["cursor"].as_str().unwrap().into(),
                limit: 2,
            },
        )
        .await;
    assert_eq!(rest["repositories"][0]["name"], "team-b/x");
    assert_eq!(rest["cursor"], "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_admins_cannot_configure_webhook_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("admin.pat"), "admin-pat").unwrap();
    let config = dir.path().join("accounts.json");
    std::fs::write(
        &config,
        br#"{"accounts":[{"name":"admin","token_file":"admin.pat","create_repositories":["team/**"],"grants":[{"repositories":["team/**"],"admin":true}]}]}"#,
    )
    .unwrap();
    let accounts = load_repository_accounts(&config).unwrap();
    let f = Fixture::with(&["team/app"], |b| {
        b.with_repository_accounts(accounts).unwrap()
    });
    let webhook_config = r#"{"webhooks":[{"id":"internal","url":"http://127.0.0.1:8080/hook"}]}"#;
    let (status, _) = f
        .call(
            Some("admin-pat"),
            "/v1/admin/update-repository",
            &p::UpdateRepositoryRequest {
                name: "team/app".into(),
                expected_config_version: f.record("team/app").config_version,
                config_json: Some(webhook_config.into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, configured) = f
        .call(
            Some("operator"),
            "/v1/admin/update-repository",
            &p::UpdateRepositoryRequest {
                name: "team/app".into(),
                expected_config_version: f.record("team/app").config_version,
                config_json: Some(webhook_config.into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{configured}");

    let (status, preserved) = f
        .call(
            Some("admin-pat"),
            "/v1/admin/update-repository",
            &p::UpdateRepositoryRequest {
                name: "team/app".into(),
                expected_config_version: f.record("team/app").config_version,
                config_json: Some(
                    r#"{"limits":{"active":2},"webhooks":[{"id":"internal","url":"http://127.0.0.1:8080/hook"}]}"#
                        .into(),
                ),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preserved}");

    let (status, _) = f
        .call(
            Some("admin-pat"),
            "/v1/admin/create-repository",
            &p::CreateRepositoryRequest {
                name: "team/new".into(),
                default_branch: b"main".to_vec(),
                config_json: webhook_config.into(),
                idempotency_key: Vec::new(),
            },
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_managed_configuration_refuses_admin_configuration_changes() {
    let f = Fixture::with(&["team/app"], |b| {
        b.with_lifecycle(Lifecycle {
            config_source: Some(ConfigSource::File),
            ..Default::default()
        })
    });
    let version = f.record("team/app").config_version;
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/admin/update-repository",
            &p::UpdateRepositoryRequest {
                name: "team/app".into(),
                expected_config_version: version,
                config_json: Some("{}".into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    // State changes remain an operator action.
    let (status, body) = f
        .call(
            Some("operator"),
            "/v1/admin/update-repository",
            &p::UpdateRepositoryRequest {
                name: "team/app".into(),
                expected_config_version: version,
                state: Some(p::RepositoryState::ReadOnly as i32),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], p::RepositoryState::ReadOnly as i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forks_share_objects_keep_git_oids_and_diverge_independently() {
    let f = Fixture::new(&["upstream/app"]);
    let first = f
        .commit("operator", "upstream/app", None, &[(b"a", b"1")])
        .await;
    let second = f
        .commit(
            "operator",
            "upstream/app",
            Some(first.clone()),
            &[(b"b", b"2")],
        )
        .await;
    let fork = || p::ForkRepositoryRequest {
        source: "upstream/app".into(),
        r#ref: b"refs/heads/main".to_vec(),
        new_name: "fork/app".into(),
        idempotency_key: b"fork-1".to_vec(),
    };
    let (status, forked) = f
        .call(Some("operator"), "/v1/admin/fork-repository", &fork())
        .await;
    assert_eq!(status, StatusCode::OK, "{forked}");
    assert_ne!(
        bytes(&forked["key"]),
        f.record("upstream/app").key.as_bytes()
    );
    let (_, upstream) = f
        .get(
            Some("operator"),
            "/upstream/app.git/info/refs?service=git-upload-pack",
        )
        .await;
    let (status, downstream) = f
        .get(
            Some("operator"),
            "/fork/app.git/info/refs?service=git-upload-pack",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(main_line(&upstream), main_line(&downstream));
    for id in [&first, &second] {
        let (status, _) = f
            .call(
                Some("operator"),
                "/v1/changeset",
                &p::ChangesetRequest {
                    repo: b"fork/app".to_vec(),
                    id: id.clone(),
                },
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    // Retrying the same fork resumes instead of failing.
    let (_, replay) = f
        .call(Some("operator"), "/v1/admin/fork-repository", &fork())
        .await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["key"], forked["key"]);
    // Later upstream work stays invisible to the fork.
    let third = f
        .commit(
            "operator",
            "upstream/app",
            Some(second.clone()),
            &[(b"c", b"3")],
        )
        .await;
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/changeset",
            &p::ChangesetRequest {
                repo: b"fork/app".to_vec(),
                id: third,
            },
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    f.commit("operator", "fork/app", Some(second), &[(b"d", b"4")])
        .await;
    let (status, usage) = f
        .call(
            Some("operator"),
            "/v1/admin/repository-usage",
            &get("fork/app"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["changesets"], 3);
    assert_eq!(usage["blobs"], 3);
    assert_eq!(usage["blob_bytes"], 3);
    assert_eq!(usage["complete"], true);
    let (_, cached) = f
        .call(
            Some("operator"),
            "/v1/admin/repository-usage",
            &get("fork/app"),
        )
        .await;
    assert_eq!(cached, usage);
}

#[test]
fn forking_an_imported_repository_keeps_its_raw_git_objects() {
    let source = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(source.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(source.path().join("file"), "imported\n").unwrap();
    git(&["add", "file"]);
    git(&["commit", "-q", "-m", "imported"]);
    let head = git(&["rev-parse", "HEAD"]).trim().to_owned();
    let f = Fixture::new(&["imported/app"]);
    let key = f.record("imported/app").key;
    let mut engine = kasu_derived::Engine::standard();
    kasu_git::register_git(&mut engine);
    let importer = kasu_ingest::Importer {
        engine: &engine,
        index: kasu_git::GitIndex {
            repo: Repository {
                meta: f.store.as_ref(),
                blobs: f.blobs.as_ref(),
            },
            projection: kasu_git::ProjectionId::full(key.as_bytes()),
        },
    };
    let report = importer.import(source.path()).unwrap();
    importer
        .publish(f.store.as_ref(), &report, b"test", 1)
        .unwrap();
    let forked = f
        .state
        .fork_repository(
            Some("Bearer operator"),
            p::ForkRepositoryRequest {
                source: "imported/app".into(),
                r#ref: b"refs/heads/main".to_vec(),
                new_name: "fork/imported".into(),
                idempotency_key: vec![],
            },
        )
        .unwrap();
    let state = f
        .state
        .scope(Some("Bearer operator"), b"fork/imported", Mode::Read)
        .unwrap();
    let id = state
        .resolve_ref(p::RefRequest {
            repo: b"fork/imported".to_vec(),
            name: b"refs/heads/main".to_vec(),
        })
        .unwrap()
        .target
        .unwrap();
    let found = state
        .find_changeset_by_git_oid(p::GitOidRequest {
            repo: b"fork/imported".to_vec(),
            oid: hex::decode(&head).unwrap(),
        })
        .unwrap();
    assert_eq!(found.id, Some(id));
    assert_eq!(forked.name, "fork/imported");
}
