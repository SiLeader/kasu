mod common;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use common::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use std::net::SocketAddr;
use tower::ServiceExt;

fn grant(patterns: &[&str], write: bool, admin: bool) -> RepositoryGrant {
    RepositoryGrant {
        repositories: patterns
            .iter()
            .map(|p| RepoPattern::parse(p).unwrap())
            .collect(),
        write_policy: write.then(|| WritePolicy {
            allow_delete: true,
            ..Default::default()
        }),
        read_directories: None,
        admin,
    }
}
fn account(name: &str, grants: Vec<RepositoryGrant>) -> RepositoryAccount {
    RepositoryAccount {
        name: name.into(),
        token: format!("{name}-pat"),
        grants,
        ..Default::default()
    }
}
fn delegate(trusted: &[&str], allow_admin: bool) -> Delegate {
    Delegate {
        name: "kasu-web".into(),
        token: "web-delegate".into(),
        act_as: vec!["alice".into(), "bob".into()],
        allow_admin,
        allow_unbound: false,
        anonymous_account: Some("public".into()),
        trusted_peers: trusted.iter().map(|c| Cidr::parse(c).unwrap()).collect(),
    }
}
fn fixture(trusted: &[&str], allow_admin: bool) -> Fixture {
    Fixture::with(&["team/app", "team/other"], |backend| {
        let mut alice = account(
            "alice",
            vec![
                grant(&["team/app"], true, true),
                grant(&["team/other"], false, false),
            ],
        );
        alice.create_repositories = vec![RepoPattern::parse("team/*").unwrap()];
        alice.display_name = Some("Alice".into());
        backend
            .with_repository_accounts(vec![
                alice,
                account("bob", vec![grant(&["team/app"], false, false)]),
                account("carol", vec![grant(&["team/**"], true, false)]),
                account("public", vec![grant(&["team/app"], true, false)]),
            ])
            .unwrap()
            .with_delegates(vec![delegate(trusted, allow_admin)])
            .unwrap()
    })
}
/// Call the HTTP API as `headers` from `peer`.
async fn call(
    fixture: &Fixture,
    headers: &[(&str, String)],
    peer: Option<&str>,
    path: &str,
    body: &impl serde::Serialize,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::post(path).header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let mut request = request
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<SocketAddr>().unwrap(),
        ));
    }
    let response = http_router(fixture.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 26).await.unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}
fn acting(account: &str, binding: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("authorization", "Bearer web-delegate".to_owned()),
        (ACT_AS, account.to_owned()),
        (ACT_USER, format!("{account}@example.com")),
    ];
    if let Some(binding) = binding {
        headers.push((ACT_BINDING, binding.to_owned()));
    }
    headers
}
async fn binding(fixture: &Fixture, account: &str) -> String {
    let (status, principal) = call(
        fixture,
        &[("authorization", format!("Bearer {account}-pat"))],
        None,
        "/v1/identity/whoami",
        &p::WhoAmIRequest {},
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{principal}");
    hex::encode(bytes(&principal["binding"]))
}
fn refs(repo: &str) -> p::ListRefsRequest {
    p::ListRefsRequest {
        repo: repo.as_bytes().to_vec(),
        prefix: vec![],
    }
}
const PEER: Option<&str> = Some("10.0.20.5:4000");

#[tokio::test]
async fn delegation_requires_trusted_peer_scope_and_matching_binding() {
    let fixture = fixture(&["10.0.20.0/24"], true);
    fixture
        .commit("operator", "team/app", None, &[(b"a", b"1")])
        .await;
    let alice = binding(&fixture, "alice").await;
    let (ok, direct) = call(
        &fixture,
        &[("authorization", "Bearer alice-pat".into())],
        None,
        "/v1/refs",
        &refs("team/app"),
    )
    .await;
    assert_eq!(ok, StatusCode::OK);
    let (status, delegated) = call(
        &fixture,
        &acting("alice", Some(&alice)),
        PEER,
        "/v1/refs",
        &refs("team/app"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delegated, direct);
    // The delegate alone reads nothing.
    let (status, _) = call(
        &fixture,
        &[("authorization", "Bearer web-delegate".into())],
        PEER,
        "/v1/refs",
        &refs("team/app"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for (headers, peer) in [
        // Outside act_as.
        (acting("carol", None), PEER),
        // Unknown account.
        (acting("mallory", None), PEER),
        // Untrusted and unknown peers.
        (acting("alice", Some(&alice)), Some("192.168.0.1:1")),
        (acting("alice", Some(&alice)), None),
        // Unbound is not allowed, and the binding belongs to alice.
        (acting("alice", None), PEER),
        (acting("bob", Some(&alice)), PEER),
    ] {
        let (status, _) = call(&fixture, &headers, peer, "/v1/refs", &refs("team/app")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{headers:?} {peer:?}");
    }
    // The internal credential cannot be sent from outside.
    let forged = format!("KasuDelegated {} 0 0 0 ", "00".repeat(32));
    let (status, _) = call(
        &fixture,
        &[("authorization", forged)],
        PEER,
        "/v1/refs",
        &refs("team/app"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Delegation metadata from a plain account token is not a delegation.
    let mut headers = acting("alice", Some(&alice));
    headers[0].1 = "Bearer bob-pat".into();
    let (status, _) = call(&fixture, &headers, PEER, "/v1/refs", &refs("team/app")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn replacing_a_pat_invalidates_existing_bindings() {
    let old = fixture(&[], true);
    let binding = binding(&old, "alice").await;
    let renewed = Fixture::with(&["team/app"], |backend| {
        let mut alice = account("alice", vec![grant(&["team/app"], true, true)]);
        alice.token = "alice-new-pat".into();
        backend
            .with_repository_accounts(vec![
                alice,
                account("public", vec![grant(&["team/app"], false, false)]),
            ])
            .unwrap()
            .with_delegates(vec![delegate(&[], true)])
            .unwrap()
    });
    // Each backend has its own random binding key unless one is configured;
    // with a shared key the old binding still fails because the PAT changed.
    let key = vec![7u8; 32];
    let a = Fixture::with(&["team/app"], |b| {
        b.with_binding_key(key.clone())
            .unwrap()
            .with_repository_accounts(vec![
                account("alice", vec![grant(&["team/app"], true, false)]),
                account("public", vec![]),
            ])
            .unwrap()
            .with_delegates(vec![delegate(&[], true)])
            .unwrap()
    });
    let first = binding_for(&a, "alice-pat").await;
    let b = Fixture::with(&["team/app"], |b| {
        let mut alice = account("alice", vec![grant(&["team/app"], true, false)]);
        alice.token = "alice-rotated".into();
        b.with_binding_key(key.clone())
            .unwrap()
            .with_repository_accounts(vec![alice, account("public", vec![])])
            .unwrap()
            .with_delegates(vec![delegate(&[], true)])
            .unwrap()
    });
    assert_ne!(first, binding_for(&b, "alice-rotated").await);
    let (status, _) = call(
        &b,
        &acting("alice", Some(&first)),
        None,
        "/v1/refs",
        &refs("team/app"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &a,
        &acting("alice", Some(&first)),
        None,
        "/v1/refs",
        &refs("team/app"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &renewed,
        &acting("alice", Some(&binding)),
        None,
        "/v1/refs",
        &refs("team/app"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
async fn binding_for(fixture: &Fixture, token: &str) -> String {
    let (_, principal) = call(
        fixture,
        &[("authorization", format!("Bearer {token}"))],
        None,
        "/v1/identity/whoami",
        &p::WhoAmIRequest {},
    )
    .await;
    hex::encode(bytes(&principal["binding"]))
}

#[tokio::test]
async fn anonymous_delegation_is_read_only_even_for_a_writable_account() {
    let fixture = fixture(&[], true);
    let base = fixture
        .commit("operator", "team/app", None, &[(b"a", b"1")])
        .await;
    let anonymous = || {
        vec![
            ("authorization", "Bearer web-delegate".to_owned()),
            (ACT_AS, "public".to_owned()),
        ]
    };
    let (status, _) = call(&fixture, &anonymous(), None, "/v1/refs", &refs("team/app")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &fixture,
        &anonymous(),
        None,
        "/v1/update-ref",
        &p::UpdateRefRequest {
            repo: b"team/app".to_vec(),
            name: b"refs/heads/copy".to_vec(),
            expected: Some(p::Ref {
                name: b"refs/heads/copy".to_vec(),
                ..Default::default()
            }),
            target: Some(base),
            force: false,
        },
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &fixture,
        &anonymous(),
        None,
        "/v1/admin/create-repository",
        &p::CreateRepositoryRequest {
            name: "team/new".into(),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, principal) = call(
        &fixture,
        &anonymous(),
        None,
        "/v1/identity/whoami",
        &p::WhoAmIRequest {},
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(principal["anonymous"], true);
    let (_, permissions) = call(
        &fixture,
        &anonymous(),
        None,
        "/v1/identity/permissions",
        &p::PermissionsRequest {
            repo: b"team/app".to_vec(),
            refs: vec![b"refs/heads/main".to_vec()],
        },
    )
    .await;
    assert_eq!(permissions["write"], false);
    assert_eq!(permissions["refs"][0]["update"], false);
}

#[tokio::test]
async fn delegated_writes_are_attributed_to_the_account_via_the_delegate() {
    let fixture = fixture(&[], true);
    let base = fixture
        .commit("operator", "team/app", None, &[(b"a", b"1")])
        .await;
    let alice = binding(&fixture, "alice").await;
    let (status, body) = call(
        &fixture,
        &acting("alice", Some(&alice)),
        None,
        "/v1/update-ref",
        &p::UpdateRefRequest {
            repo: b"team/app".to_vec(),
            name: b"refs/heads/topic".to_vec(),
            expected: Some(p::Ref {
                name: b"refs/heads/topic".to_vec(),
                ..Default::default()
            }),
            target: Some(base),
            force: false,
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    use kasu_store::RefStore;
    let key = fixture.record("team/app").key;
    let log = fixture.store.tail(key.as_bytes(), 0, 10).unwrap();
    let entry = log.last().unwrap();
    assert_eq!(entry.actor, b"alice");
    assert_eq!(entry.via, b"kasu-web:alice@example.com");
}

#[tokio::test]
async fn identity_and_permissions_reflect_grants_without_revealing_invisible_repositories() {
    let fixture = fixture(&[], true);
    let (status, principal) = call(
        &fixture,
        &[("authorization", "Bearer alice-pat".into())],
        None,
        "/v1/identity/whoami",
        &p::WhoAmIRequest {},
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(principal["account"], "alice");
    assert_eq!(principal["create_repositories"][0], "team/*");
    assert_eq!(bytes(&principal["display_name"]), b"Alice");
    let permissions = |token: &'static str, repo: &'static str| {
        let fixture = &fixture;
        async move {
            call(
                fixture,
                &[("authorization", format!("Bearer {token}"))],
                None,
                "/v1/identity/permissions",
                &p::PermissionsRequest {
                    repo: repo.as_bytes().to_vec(),
                    refs: vec![b"refs/heads/main".to_vec(), b"refs/notes/x".to_vec()],
                },
            )
            .await
        }
    };
    let (status, alice) = permissions("alice-pat", "team/app").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(alice["write"], true);
    assert_eq!(alice["admin"], true);
    assert_eq!(alice["refs"][0]["update"], true);
    assert_eq!(alice["refs"][1]["update"], false);
    let (_, other) = permissions("alice-pat", "team/other").await;
    assert_eq!(other["write"], false);
    assert_eq!(other["admin"], false);
    let (missing, _) = permissions("bob-pat", "team/other").await;
    let (absent, _) = permissions("bob-pat", "team/none").await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
    assert_eq!(absent, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn repository_administration_follows_admin_grants_and_create_patterns() {
    let fixture = fixture(&[], false);
    let alice = |path: &'static str, body: serde_json::Value| {
        let fixture = &fixture;
        async move {
            call(
                fixture,
                &[("authorization", "Bearer alice-pat".into())],
                None,
                path,
                &body,
            )
            .await
        }
    };
    let (status, created) = alice(
        "/v1/admin/create-repository",
        serde_json::json!({"name": "team/new"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(bytes(&created["created_by"]), b"alice");
    let (status, _) = alice(
        "/v1/admin/create-repository",
        serde_json::json!({"name": "elsewhere/new"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, repo) = alice(
        "/v1/admin/get-repository",
        serde_json::json!({"name": "team/app"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let version = repo["config_version"].as_u64().unwrap();
    // Admins see and change the configuration of repositories they administer.
    let (status, updated) = alice(
        "/v1/admin/update-repository",
        serde_json::json!({"name": "team/app", "expected_config_version": version,
            "config_json": r#"{"limits":{"active":2}}"#}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert!(updated["config_json"].as_str().unwrap().contains("active"));
    let (status, _) = alice(
        "/v1/admin/update-repository",
        serde_json::json!({"name": "team/other", "expected_config_version": 0, "state": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Secrets outside KASU_WEBHOOK_SECRET_DIR are refused for non-operators.
    let (status, _) = alice(
        "/v1/admin/update-repository",
        serde_json::json!({"name": "team/app", "expected_config_version": version + 1,
            "config_json": r#"{"webhooks":[{"id":"x","url":"https://hook.example/","secret_file":"/etc/shadow"}]}"#}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = alice(
        "/v1/admin/rename-repository",
        serde_json::json!({"name": "team/app", "new_name": "other/app"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // allow_admin=false: delegated administration is refused, reads are not.
    let binding = binding(&fixture, "alice").await;
    let (status, _) = call(
        &fixture,
        &acting("alice", Some(&binding)),
        None,
        "/v1/admin/delete-repository",
        &serde_json::json!({"name": "team/app"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, list) = call(
        &fixture,
        &acting("alice", Some(&binding)),
        None,
        "/v1/admin/list-repositories",
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list["repositories"][0]["config_json"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    let (status, _) = alice(
        "/v1/admin/delete-repository",
        serde_json::json!({"name": "team/app"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Every account × endpoint: the delegated status and body equal the direct
/// PAT status and body (anonymous excepted, which is read-only by design).
#[tokio::test]
async fn delegated_results_match_direct_results_across_endpoints() {
    let fixture = fixture(&[], true);
    let base = fixture
        .commit("operator", "team/app", None, &[(b"a", b"1")])
        .await;
    let calls: Vec<(&str, serde_json::Value)> = vec![
        ("/v1/refs", serde_json::to_value(refs("team/app")).unwrap()),
        (
            "/v1/refs",
            serde_json::to_value(refs("team/other")).unwrap(),
        ),
        (
            "/v1/read-file",
            serde_json::to_value(p::PathRequest {
                repo: b"team/app".to_vec(),
                id: base.clone(),
                path: b"a".to_vec(),
                ..Default::default()
            })
            .unwrap(),
        ),
        (
            "/v1/changeset",
            serde_json::to_value(p::ChangesetRequest {
                repo: b"team/app".to_vec(),
                id: base.clone(),
            })
            .unwrap(),
        ),
        (
            "/v1/update-ref",
            serde_json::to_value(p::UpdateRefRequest {
                repo: b"team/app".to_vec(),
                name: b"refs/heads/main".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/main".to_vec(),
                    target: Some(vec![0; 32]),
                    version: 99,
                }),
                target: Some(base.clone()),
                force: false,
            })
            .unwrap(),
        ),
        (
            "/v1/admin/get-repository",
            serde_json::json!({"name": "team/app"}),
        ),
        ("/v1/admin/list-repositories", serde_json::json!({})),
        (
            "/v1/admin/update-repository",
            serde_json::json!({"name": "team/other", "expected_config_version": 99, "state": 1}),
        ),
        (
            "/v1/identity/permissions",
            serde_json::to_value(p::PermissionsRequest {
                repo: b"team/app".to_vec(),
                refs: vec![b"refs/heads/main".to_vec()],
            })
            .unwrap(),
        ),
        (
            "/v1/review/list",
            serde_json::to_value(p::ListReviewsRequest {
                repo: b"team/app".to_vec(),
                ..Default::default()
            })
            .unwrap(),
        ),
    ];
    for account in ["alice", "bob"] {
        let binding = binding(&fixture, account).await;
        for (path, body) in &calls {
            let direct = call(
                &fixture,
                &[("authorization", format!("Bearer {account}-pat"))],
                None,
                path,
                body,
            )
            .await;
            let delegated =
                call(&fixture, &acting(account, Some(&binding)), None, path, body).await;
            assert_eq!(direct, delegated, "{account} {path}");
        }
    }
}
