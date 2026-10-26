mod common;
use axum::http::StatusCode;
use common::*;
use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repositories_share_objects_but_never_expose_each_other() {
    let f = Fixture::new(&["team-a/infra", "team-b/app"]);
    let a = f
        .commit(
            "operator",
            "team-a/infra",
            None,
            &[(b"secret", b"only in a"), (b"shared", b"same bytes")],
        )
        .await;
    let b = f
        .commit(
            "operator",
            "team-b/app",
            None,
            &[(b"shared", b"same bytes")],
        )
        .await;
    // Content-addressed storage is shared: one blob frame serves both.
    let shared = BlobId(ObjectId::hash(b"same bytes").0);
    let secret = BlobId(ObjectId::hash(b"only in a").0);
    let repo = Repository {
        meta: f.store.as_ref(),
        blobs: f.blobs.as_ref(),
    };
    for repository in ["team-a/infra", "team-b/app"] {
        assert!(
            repo.has_blob_membership(f.record(repository).key.as_bytes(), shared)
                .unwrap()
        );
    }
    assert!(
        !repo
            .has_blob_membership(f.record("team-b/app").key.as_bytes(), secret)
            .unwrap()
    );

    // Another repository's changeset, blob and refs look exactly like missing ones.
    let missing = vec![7u8; 32];
    for (id, label) in [(a.clone(), "foreign"), (missing.clone(), "missing")] {
        let (status, body) = f
            .call(
                Some("operator"),
                "/v1/changeset",
                &p::ChangesetRequest {
                    repo: b"team-b/app".to_vec(),
                    id,
                },
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{label}");
        assert_eq!(body["code"], tonic::Code::NotFound as i32);
    }
    let foreign = f
        .call(
            Some("operator"),
            "/v1/blob-metadata",
            &p::ObjectRequest {
                repo: b"team-b/app".to_vec(),
                ids: vec![secret.0.to_vec()],
            },
        )
        .await;
    let absent = f
        .call(
            Some("operator"),
            "/v1/blob-metadata",
            &p::ObjectRequest {
                repo: b"team-b/app".to_vec(),
                ids: vec![missing.clone()],
            },
        )
        .await;
    assert_eq!(foreign.0, StatusCode::NOT_FOUND);
    assert_eq!(foreign.0, absent.0);
    // Deduplication is no existence oracle: probing for a blob that only another
    // repository has still demands the content and fails like an unknown ID.
    let probe = |id: Vec<u8>| p::UploadBlobsHttpRequest {
        chunks: vec![p::BlobUpload {
            repo: b"team-b/app".to_vec(),
            id,
            offset: 0,
            data: vec![],
            last: true,
        }],
    };
    let foreign = f
        .call(
            Some("operator"),
            "/v1/upload-blobs",
            &probe(secret.0.to_vec()),
        )
        .await;
    let absent = f
        .call(
            Some("operator"),
            "/v1/upload-blobs",
            &probe(missing.clone()),
        )
        .await;
    assert_eq!(foreign.0, StatusCode::BAD_REQUEST);
    assert_eq!(foreign, absent);
    // Sending the bytes adds only the membership; the existing frame is reused.
    let (status, receipt) = f
        .call(
            Some("operator"),
            "/v1/upload-blobs",
            &p::UploadBlobsHttpRequest {
                chunks: vec![p::BlobUpload {
                    repo: b"team-b/app".to_vec(),
                    id: secret.0.to_vec(),
                    offset: 0,
                    data: b"only in a".to_vec(),
                    last: true,
                }],
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["blobs"][0]["existing"], false);
    // A changeset cannot name a parent from another repository.
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/create-changeset",
            &p::CreateChangesetRequest {
                repo: b"team-b/app".to_vec(),
                parents: vec![a.clone()],
                changes: vec![],
                metadata: Some(p::CommitMetadata {
                    author: Some(p::Signature::default()),
                    committer: Some(p::Signature::default()),
                    message: vec![],
                }),
            },
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, refs) = f
        .call(
            Some("operator"),
            "/v1/refs",
            &p::ListRefsRequest {
                repo: b"team-b/app".to_vec(),
                prefix: vec![],
            },
        )
        .await;
    assert_eq!(refs["refs"].as_array().unwrap().len(), 1);
    assert_eq!(bytes(&refs["refs"][0]["target"]), b);
    // Git URLs carry multi-segment names; each repository advertises its own refs.
    let (status, advert) = f
        .get(
            Some("operator"),
            "/team-a/infra.git/info/refs?service=git-upload-pack",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, other) = f
        .get(
            Some("operator"),
            "/team-b/app/info/refs?service=git-upload-pack",
        )
        .await;
    assert_ne!(advert, other);
    assert!(advertisement(&advert).contains("symref=HEAD:refs/heads/main"));
    // SSH takes the repository from the client's command.
    let ssh = |state: &Backend, command: &str| {
        let mut output = Vec::new();
        serve_git_ssh(
            state,
            "Bearer operator",
            command.as_bytes(),
            None,
            &mut std::io::Cursor::new(Vec::new()),
            &mut output,
        )
        .map(|_| output)
    };
    let over_ssh = ssh(&f.state, "git-upload-pack '/team-a/infra.git'").unwrap();
    assert!(advert.ends_with(&over_ssh));
    assert!(other.ends_with(&ssh(&f.state, "git-upload-pack 'team-b/app'").unwrap()));
    assert!(ssh(&f.state, "git-upload-pack 'team-c/none.git'").is_err());
    let pinned = (*f.state)
        .clone()
        .with_pinned_repository(f.record("team-b/app").key);
    assert!(ssh(&pinned, "git-upload-pack 'team-a/infra.git'").is_err());
    ssh(&pinned, "git-upload-pack 'team-b/app.git'").unwrap();
    for path in [
        "/team-c/none.git/info/refs?service=git-upload-pack",
        "/team-a.git/info/refs?service=git-upload-pack",
        "/metrics.git/info/refs?service=git-upload-pack",
    ] {
        assert_eq!(
            f.get(Some("operator"), path).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_aliases_default_branch_states_and_deletion_follow_the_registry() {
    let f = Fixture::new(&["team-a/infra"]);
    let first = f
        .commit("operator", "team-a/infra", None, &[(b"file", b"v1")])
        .await;
    let (_, before) = f
        .get(
            Some("operator"),
            "/team-a/infra.git/info/refs?service=git-upload-pack",
        )
        .await;
    // Rename keeping an alias: the old URL still resolves, case-insensitively,
    // to the same key, so the advertised Git OIDs are unchanged.
    let record = f.record("team-a/infra");
    assert!(matches!(
        f.store
            .rename_repository(
                &record.key,
                record.version,
                &name("team-a/platform"),
                Some(i64::MAX),
                b"test",
                2,
            )
            .unwrap(),
        RepositoryWrite::Applied(_)
    ));
    std::thread::sleep(std::time::Duration::from_millis(5));
    for path in [
        "/team-a/platform.git/info/refs?service=git-upload-pack",
        "/team-a/infra.git/info/refs?service=git-upload-pack",
        "/Team-A/Infra/info/refs?service=git-upload-pack",
    ] {
        let (status, body) = f.get(Some("operator"), path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body, before, "{path}");
    }
    // The default branch comes from the registry record.
    f.update("team-a/platform", |r| r.default_branch = b"trunk".to_vec());
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (_, advert) = f
        .get(
            Some("operator"),
            "/team-a/platform.git/info/refs?service=git-upload-pack",
        )
        .await;
    assert!(advertisement(&advert).contains("symref=HEAD:refs/heads/trunk"));

    // ReadOnly keeps reads and refuses every write before validation.
    f.update("team-a/platform", |r| r.state = RepositoryState::ReadOnly);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/changeset",
            &p::ChangesetRequest {
                repo: b"team-a/platform".to_vec(),
                id: first.clone(),
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = f
        .call(
            Some("operator"),
            "/v1/update-ref",
            &p::UpdateRefRequest {
                repo: b"team-a/platform".to_vec(),
                name: b"refs/heads/main".to_vec(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(body["code"], tonic::Code::FailedPrecondition as i32);
    // A stale cached Active record cannot land: the ref transaction re-checks.
    let key = f.record("team-a/platform").key;
    assert!(matches!(
        f.store.compare_and_swap(
            key.as_bytes(),
            &[RefUpdate {
                name: b"refs/heads/x".to_vec(),
                expected: RefState::default(),
                new: Some(ChangesetId(first.clone().try_into().unwrap())),
            }],
            b"stale",
            3
        ),
        Err(Error::Precondition(_))
    ));

    // Deleting hides the repository and frees its name and aliases.
    f.update("team-a/platform", |r| {
        r.state = RepositoryState::Deleting;
        r.deleted_at = 4;
    });
    std::thread::sleep(std::time::Duration::from_millis(5));
    for path in [
        "/team-a/platform.git/info/refs?service=git-upload-pack",
        "/team-a/infra.git/info/refs?service=git-upload-pack",
    ] {
        assert_eq!(f.get(Some("operator"), path).await.0, StatusCode::NOT_FOUND);
    }
    // Recreating the name gets a new key: no former ref or object is visible.
    let record = RepositoryRecord::new(
        RepoKey::generate().unwrap(),
        name("team-a/platform"),
        b"test",
        5,
    );
    assert!(matches!(
        f.store.create_repository(&record).unwrap(),
        RepositoryWrite::Applied(_)
    ));
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (_, refs) = f
        .call(
            Some("operator"),
            "/v1/refs",
            &p::ListRefsRequest {
                repo: b"team-a/platform".to_vec(),
                prefix: vec![],
            },
        )
        .await;
    assert!(refs["refs"].as_array().is_none_or(Vec::is_empty));
    let (status, _) = f
        .call(
            Some("operator"),
            "/v1/changeset",
            &p::ChangesetRequest {
                repo: b"team-a/platform".to_vec(),
                id: first,
            },
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grants_choose_the_most_specific_match_and_hide_other_repositories() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bot.pat"), "deploy-bot-pat").unwrap();
    std::fs::write(dir.path().join("legacy.pat"), "legacy-pat").unwrap();
    let config = dir.path().join("accounts.json");
    std::fs::write(
        &config,
        br#"{"accounts":[
            {"name":"deploy-bot","token_file":"bot.pat","grants":[
                {"repositories":["team-a/*"],"write_policy":{"allowed_ref_patterns":["refs/heads/*"]}},
                {"repositories":["team-a/locked"]},
                {"repositories":["shared/charts"],"read_directories":["charts/team-a"]}
            ]},
            {"name":"legacy","token_file":"legacy.pat","write_policy":{}}
        ]}"#,
    )
    .unwrap();
    let accounts = load_repository_accounts(&config).unwrap();
    let f = Fixture::with(
        &[
            "team-a/infra",
            "team-a/locked",
            "shared/charts",
            "team-c/private",
            "old",
        ],
        |backend| backend.with_repository_accounts(accounts).unwrap(),
    );
    let legacy = f.record("old").key;
    let f = Fixture {
        state: Arc::new((*f.state).clone().with_legacy_repository(legacy)),
        ..f
    };
    f.commit("deploy-bot-pat", "team-a/infra", None, &[(b"x", b"1")])
        .await;
    f.commit("legacy-pat", "old", None, &[(b"x", b"1")]).await;
    let refs = |repo: &str| p::ListRefsRequest {
        repo: repo.as_bytes().to_vec(),
        prefix: vec![],
    };
    // The exact grant is read-only even though the glob grant may write.
    let (status, _) = f
        .call(Some("deploy-bot-pat"), "/v1/refs", &refs("team-a/locked"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = f
        .call(
            Some("deploy-bot-pat"),
            "/v1/update-ref",
            &p::UpdateRefRequest {
                repo: b"team-a/locked".to_vec(),
                name: b"refs/heads/main".to_vec(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Without a grant, a repository is indistinguishable from a missing one.
    let hidden = f
        .call(Some("deploy-bot-pat"), "/v1/refs", &refs("team-c/private"))
        .await;
    let missing = f
        .call(Some("deploy-bot-pat"), "/v1/refs", &refs("team-c/missing"))
        .await;
    assert_eq!(hidden.0, StatusCode::NOT_FOUND);
    assert_eq!(hidden, missing);
    assert_eq!(
        f.get(
            Some("deploy-bot-pat"),
            "/team-c/private.git/info/refs?service=git-upload-pack"
        )
        .await,
        f.get(
            Some("deploy-bot-pat"),
            "/team-c/missing.git/info/refs?service=git-upload-pack"
        )
        .await
    );
    // Old-format accounts are a single grant on the legacy repository only.
    assert_eq!(
        f.call(Some("legacy-pat"), "/v1/refs", &refs("old")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        f.call(Some("legacy-pat"), "/v1/refs", &refs("team-a/infra"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // Directory grants restrict native reads and Git to readable projections.
    assert_eq!(
        f.get(
            Some("deploy-bot-pat"),
            "/shared/charts.git/info/refs?service=git-upload-pack"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.get(
            Some("deploy-bot-pat"),
            "/shared/charts/-/path/charts/team-a.git/info/refs?service=git-upload-pack"
        )
        .await
        .0,
        StatusCode::OK
    );
    // Grants follow the key: a rename keeps access, and the old-name alias
    // does not grant access to a repository the account cannot otherwise see.
    let record = f.record("team-a/infra");
    f.store
        .rename_repository(
            &record.key,
            record.version,
            &name("platform/infra"),
            Some(i64::MAX),
            b"test",
            9,
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    for repo in ["platform/infra", "team-a/infra"] {
        assert_eq!(
            f.call(Some("deploy-bot-pat"), "/v1/refs", &refs(repo))
                .await
                .0,
            StatusCode::OK,
            "{repo}"
        );
    }
    let private = f.record("team-c/private");
    f.store
        .rename_repository(
            &private.key,
            private.version,
            &name("team-c/renamed"),
            Some(i64::MAX),
            b"test",
            9,
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(
        f.call(Some("deploy-bot-pat"), "/v1/refs", &refs("team-c/private"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrating_a_single_repository_keeps_every_key_and_git_oid() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(FsBlobStore::open(dir.path()).unwrap());
    let legacy = Fixture {
        _dir: tempfile::tempdir().unwrap(),
        store: store.clone(),
        blobs: blobs.clone(),
        state: Arc::new(Backend::new(
            b"demo".to_vec(),
            store.clone(),
            blobs.clone(),
            store.clone(),
            Some("operator".into()),
        )),
    };
    let first = legacy
        .commit("operator", "demo", None, &[(b"a", b"1")])
        .await;
    legacy
        .commit("operator", "demo", Some(first), &[(b"b", b"2")])
        .await;
    let (_, before) = legacy
        .get(
            Some("operator"),
            "/demo.git/info/refs?service=git-upload-pack",
        )
        .await;
    // Migration registers the name bytes as the key and is idempotent.
    let record = migrate_registry(store.as_ref(), &name("demo"), b"test").unwrap();
    assert_eq!(record.key.as_bytes(), b"demo");
    assert_eq!(
        migrate_registry(store.as_ref(), &name("demo"), b"test").unwrap(),
        record
    );
    assert_eq!(store.list_repositories(None, 10).unwrap().len(), 1);
    let migrated = Fixture {
        _dir: tempfile::tempdir().unwrap(),
        store: store.clone(),
        blobs: blobs.clone(),
        state: Arc::new(Backend::with_registry(
            store.clone(),
            blobs.clone(),
            store.clone(),
            store.clone(),
            Some("operator".into()),
        )),
    };
    let (status, after) = migrated
        .get(
            Some("operator"),
            "/demo.git/info/refs?service=git-upload-pack",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before, after);
    // A non-empty registry only accepts registered startup names.
    assert!(legacy_repository(store.as_ref(), &name("other")).is_err());
    assert_eq!(
        legacy_repository(store.as_ref(), &name("demo")).unwrap(),
        record
    );
    let empty = SqliteStore::memory().unwrap();
    assert_eq!(
        legacy_repository(&empty, &name("fresh"))
            .unwrap()
            .key
            .as_bytes(),
        b"fresh"
    );
}
