mod common;
use axum::http::StatusCode;
use common::*;
use kasu_core::ObjectId;
use kasu_proto::v1 as p;
use kasu_server::*;

fn grant(write: bool, dirs: Option<&[u8]>) -> RepositoryGrant {
    RepositoryGrant {
        repositories: vec![RepoPattern::parse("**").unwrap()],
        write_policy: write.then(WritePolicy::default),
        read_directories: dirs.map(|d| vec![kasu_core::RepoPath::new(d).unwrap()]),
        admin: false,
    }
}
fn account(name: &str, write: bool) -> RepositoryAccount {
    RepositoryAccount {
        name: name.into(),
        token: name.into(),
        grants: vec![grant(write, None)],
        ..Default::default()
    }
}
const POLICY: &str = r#"{"review":{"rules":[{"ref_patterns":["refs/heads/main"],"required_approvals":1}],"bypass_accounts":["bot"]}}"#;
fn fixture(policy: &str) -> Fixture {
    let fixture = Fixture::with(&["app"], |backend| {
        let mut dave = account("dave", true);
        dave.grants = vec![grant(false, Some(b"docs"))];
        backend
            .with_repository_accounts(vec![
                account("alice", true),
                account("bob", true),
                account("carol", false),
                account("bot", true),
                dave,
            ])
            .unwrap()
    });
    let config = policy.as_bytes().to_vec();
    fixture.update("app", |r| {
        r.config = config;
        r.config_version += 1;
    });
    fixture
}
async fn change(
    fixture: &Fixture,
    token: &str,
    parent: Vec<u8>,
    path: &[u8],
    content: &[u8],
) -> Vec<u8> {
    let (status, _) = fixture
        .call(
            Some(token),
            "/v1/upload-blobs",
            &p::UploadBlobsHttpRequest {
                chunks: vec![p::BlobUpload {
                    repo: b"app".to_vec(),
                    id: ObjectId::hash(content).0.to_vec(),
                    offset: 0,
                    data: content.to_vec(),
                    last: true,
                }],
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let signature = p::Signature {
        name: token.as_bytes().to_vec(),
        email: b"x@example.com".to_vec(),
        timestamp: 2,
        tz: b"+0000".to_vec(),
    };
    let (status, cs) = fixture
        .call(
            Some(token),
            "/v1/create-changeset",
            &p::CreateChangesetRequest {
                repo: b"app".to_vec(),
                parents: vec![parent],
                changes: vec![p::FileChange {
                    path: path.to_vec(),
                    entry: Some(p::Entry {
                        kind: Some(p::entry::Kind::File(p::File {
                            blob: ObjectId::hash(content).0.to_vec(),
                            executable: false,
                        })),
                    }),
                }],
                metadata: Some(p::CommitMetadata {
                    author: Some(signature.clone()),
                    committer: Some(signature),
                    message: format!("change {}", String::from_utf8_lossy(path)).into_bytes(),
                }),
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cs}");
    bytes(&cs["id"])
}
async fn create(fixture: &Fixture, token: &str, stack: Vec<Vec<u8>>) -> serde_json::Value {
    let (status, review) = fixture
        .call(
            Some(token),
            "/v1/review/create",
            &p::CreateReviewRequest {
                repo: b"app".to_vec(),
                target: b"refs/heads/main".to_vec(),
                changesets: stack,
                title: b"Add files".to_vec(),
                description: b"Body".to_vec(),
                idempotency_key: b"create-1".to_vec(),
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{review}");
    review
}
async fn vote(fixture: &Fixture, token: &str, revision: u32, value: p::VoteValue) {
    let (status, body) = fixture
        .call(
            Some(token),
            "/v1/review/publish",
            &p::PublishRequest {
                repo: b"app".to_vec(),
                number: 1,
                revision,
                vote: Some(value as i32),
                body: vec![],
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
async fn submit_status(fixture: &Fixture, token: &str) -> serde_json::Value {
    let (status, body) = fixture
        .call(
            Some(token),
            "/v1/review/submit-status",
            &p::GetReviewRequest {
                repo: b"app".to_vec(),
                number: 1,
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}
fn submit(revision: u32, method: p::SubmitMethod, key: &[u8]) -> p::SubmitRequest {
    p::SubmitRequest {
        repo: b"app".to_vec(),
        number: 1,
        expected_revision: revision,
        method: method as i32,
        idempotency_key: key.to_vec(),
        ..Default::default()
    }
}
fn main_ref(fixture: &Fixture) -> kasu_store::RefState {
    use kasu_store::RefStore;
    fixture
        .store
        .resolve(fixture.record("app").key.as_bytes(), b"refs/heads/main")
        .unwrap()
}

#[tokio::test]
async fn protected_refs_reject_direct_updates_except_for_bypass_accounts() {
    let fixture = fixture(POLICY);
    let base = fixture
        .commit("operator", "app", None, &[(b"a", b"1")])
        .await;
    let next = change(&fixture, "alice", base.clone(), b"b", b"2").await;
    let version = main_ref(&fixture).version;
    let update = |target: Vec<u8>, version| p::UpdateRefRequest {
        repo: b"app".to_vec(),
        name: b"refs/heads/main".to_vec(),
        expected: Some(p::Ref {
            name: b"refs/heads/main".to_vec(),
            target: Some(base.clone()),
            version,
        }),
        target: Some(target),
        force: false,
    };
    let (status, body) = fixture
        .call(
            Some("alice"),
            "/v1/update-ref",
            &update(next.clone(), version),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("review required")
    );
    let (status, land) = fixture
        .call(
            Some("alice"),
            "/v1/land",
            &p::LandRequest {
                repo: b"app".to_vec(),
                id: next.clone(),
                name: b"refs/heads/main".to_vec(),
                idempotency_key: b"land".to_vec(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(land["outcome"], p::LandOutcome::Denied as i32);
    assert_eq!(land["message"], "review required");
    // Other refs stay writable, and the bypass account updates main.
    let (status, _) = fixture
        .call(
            Some("alice"),
            "/v1/update-ref",
            &p::UpdateRefRequest {
                name: b"refs/heads/topic".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/topic".to_vec(),
                    ..Default::default()
                }),
                ..update(next.clone(), 0)
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = fixture
        .call(Some("bot"), "/v1/update-ref", &update(next, version))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    use kasu_store::RefStore;
    let log = fixture
        .store
        .tail(fixture.record("app").key.as_bytes(), 0, 10)
        .unwrap();
    assert_eq!(log.last().unwrap().via, b"bypass");
    let (_, permissions) = fixture
        .call(
            Some("alice"),
            "/v1/identity/permissions",
            &p::PermissionsRequest {
                repo: b"app".to_vec(),
                refs: vec![b"refs/heads/main".to_vec()],
            },
        )
        .await;
    assert_eq!(permissions["refs"][0]["review_required"], true);
    assert_eq!(permissions["review"], true);
}

#[tokio::test]
async fn approved_stack_submits_once_with_rebase_and_trailers() {
    let fixture = fixture(POLICY);
    let base = fixture
        .commit("operator", "app", None, &[(b"a", b"1")])
        .await;
    let first = change(&fixture, "alice", base.clone(), b"b", b"2").await;
    let second = change(&fixture, "alice", first.clone(), b"c", b"3").await;
    let review = create(&fixture, "alice", vec![first.clone(), second.clone()]).await;
    assert_eq!(review["number"], 1);
    // Same key: same review; another key with the same head is a duplicate.
    let replay = create(&fixture, "alice", vec![first.clone(), second.clone()]).await;
    assert_eq!(replay["replayed"], true);
    let (status, _) = fixture
        .call(
            Some("alice"),
            "/v1/review/create",
            &p::CreateReviewRequest {
                repo: b"app".to_vec(),
                target: b"refs/heads/main".to_vec(),
                changesets: vec![first.clone(), second.clone()],
                title: b"again".to_vec(),
                idempotency_key: b"other".to_vec(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // A read-only account comments but cannot create.
    let (status, _) = fixture
        .call(
            Some("carol"),
            "/v1/review/create",
            &p::CreateReviewRequest {
                repo: b"app".to_vec(),
                target: b"refs/heads/main".to_vec(),
                changesets: vec![first.clone()],
                title: b"t".to_vec(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Self-approval and approvals from non-writers do not count.
    vote(&fixture, "alice", 1, p::VoteValue::Approve).await;
    vote(&fixture, "carol", 1, p::VoteValue::Approve).await;
    let current = submit_status(&fixture, "alice").await;
    assert_eq!(current["ready"], false);
    assert_eq!(current["approvals"], 0);
    let (_, early) = fixture
        .call(
            Some("alice"),
            "/v1/review/submit",
            &submit(1, p::SubmitMethod::Rebase, b"s"),
        )
        .await;
    assert_eq!(early["outcome"], p::SubmitOutcome::NotReady as i32);
    // The target moves meanwhile; Submit rebases the stack onto it.
    let moved = change(&fixture, "bot", base.clone(), b"z", b"9").await;
    let version = main_ref(&fixture).version;
    let (status, _) = fixture
        .call(
            Some("bot"),
            "/v1/update-ref",
            &p::UpdateRefRequest {
                repo: b"app".to_vec(),
                name: b"refs/heads/main".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/main".to_vec(),
                    target: Some(base.clone()),
                    version,
                }),
                target: Some(moved.clone()),
                force: false,
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    vote(&fixture, "bob", 1, p::VoteValue::Approve).await;
    assert_eq!(status_of(&fixture).await["ready"], true);
    let (status, landed) = fixture
        .call(
            Some("alice"),
            "/v1/review/submit",
            &submit(1, p::SubmitMethod::Rebase, b"s"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{landed}");
    assert_eq!(
        landed["outcome"],
        p::SubmitOutcome::Landed as i32,
        "{landed}"
    );
    let ids: Vec<Vec<u8>> = landed["landed"]
        .as_array()
        .unwrap()
        .iter()
        .map(bytes)
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(main_ref(&fixture).target.unwrap().0.to_vec(), ids[1]);
    let (_, tip) = fixture
        .call(
            Some("alice"),
            "/v1/changeset",
            &p::ChangesetRequest {
                repo: b"app".to_vec(),
                id: ids[1].clone(),
            },
        )
        .await;
    assert_eq!(bytes(&tip["parents"][0]), ids[0]);
    assert!(
        String::from_utf8(bytes(&tip["message"]))
            .unwrap()
            .ends_with("Review: app#1\n")
    );
    let (_, first_landed) = fixture
        .call(
            Some("alice"),
            "/v1/changeset",
            &p::ChangesetRequest {
                repo: b"app".to_vec(),
                id: ids[0].clone(),
            },
        )
        .await;
    assert_eq!(bytes(&first_landed["parents"][0]), moved);
    assert_eq!(bytes(&first_landed["author"]["name"]), b"alice");
    let (_, again) = fixture
        .call(
            Some("alice"),
            "/v1/review/submit",
            &submit(1, p::SubmitMethod::Rebase, b"s"),
        )
        .await;
    assert_eq!(again["replayed"], true);
    assert_eq!(again["landed"], landed["landed"]);
    let (_, review) = fixture
        .call(
            Some("carol"),
            "/v1/review/get",
            &p::GetReviewRequest {
                repo: b"app".to_vec(),
                number: 1,
            },
        )
        .await;
    assert_eq!(review["state"], p::ReviewState::Submitted as i32);
    let (_, other) = fixture
        .call(
            Some("alice"),
            "/v1/review/submit",
            &submit(1, p::SubmitMethod::Rebase, b"t"),
        )
        .await;
    assert_eq!(other["outcome"], p::SubmitOutcome::NotReady as i32);
}
async fn status_of(fixture: &Fixture) -> serde_json::Value {
    submit_status(fixture, "alice").await
}

#[tokio::test]
async fn approvals_survive_only_trivial_rebases_and_request_changes_blocks() {
    let fixture = fixture(POLICY);
    let base = fixture
        .commit("operator", "app", None, &[(b"a", b"1")])
        .await;
    let first = change(&fixture, "alice", base.clone(), b"b", b"2").await;
    create(&fixture, "alice", vec![first.clone()]).await;
    vote(&fixture, "bob", 1, p::VoteValue::Approve).await;
    // Move the target, then follow it: the content of the change is the same.
    let moved = change(&fixture, "bot", base.clone(), b"z", b"9").await;
    let version = main_ref(&fixture).version;
    fixture
        .call(
            Some("bot"),
            "/v1/update-ref",
            &p::UpdateRefRequest {
                repo: b"app".to_vec(),
                name: b"refs/heads/main".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/main".to_vec(),
                    target: Some(base.clone()),
                    version,
                }),
                target: Some(moved),
                force: false,
            },
        )
        .await;
    let (status, rebased) = fixture
        .call(
            Some("alice"),
            "/v1/review/rebase",
            &p::RebaseReviewRequest {
                repo: b"app".to_vec(),
                number: 1,
                expected_revision: 1,
                onto: vec![],
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rebased}");
    let revisions = rebased["review"]["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0]["fingerprint"], revisions[1]["fingerprint"]);
    assert_eq!(status_of(&fixture).await["ready"], true);
    // A changed stack needs a new approval.
    let tip = bytes(&revisions[1]["changesets"][0]);
    let edited = change(&fixture, "alice", tip, b"b", b"edited").await;
    let (status, body) = fixture
        .call(
            Some("alice"),
            "/v1/review/add-revision",
            &p::AddRevisionRequest {
                repo: b"app".to_vec(),
                number: 1,
                expected_revision: 2,
                changesets: vec![bytes(&revisions[1]["changesets"][0]), edited],
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(status_of(&fixture).await["ready"], false);
    vote(&fixture, "bob", 3, p::VoteValue::Approve).await;
    assert_eq!(status_of(&fixture).await["ready"], true);
    vote(&fixture, "bot", 3, p::VoteValue::RequestChanges).await;
    let blocked = status_of(&fixture).await;
    assert_eq!(blocked["ready"], false);
    assert!(
        blocked["unmet"]
            .to_string()
            .contains("bot requested changes")
    );
    // Stale expectations are refused.
    let (status, _) = fixture
        .call(
            Some("alice"),
            "/v1/review/add-revision",
            &p::AddRevisionRequest {
                repo: b"app".to_vec(),
                number: 1,
                expected_revision: 2,
                changesets: vec![first],
            },
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn squash_submit_lands_one_changeset_with_the_review_message() {
    let fixture = fixture(
        r#"{"review":{"rules":[{"ref_patterns":["refs/heads/main"],"required_approvals":0,"methods":["SQUASH"]}]}}"#,
    );
    let base = fixture
        .commit("operator", "app", None, &[(b"a", b"1")])
        .await;
    let first = change(&fixture, "alice", base.clone(), b"b", b"2").await;
    let second = change(&fixture, "alice", first.clone(), b"c", b"3").await;
    create(&fixture, "alice", vec![first, second]).await;
    let (_, denied) = fixture
        .call(
            Some("alice"),
            "/v1/review/submit",
            &submit(1, p::SubmitMethod::Rebase, b"r"),
        )
        .await;
    assert_eq!(denied["outcome"], p::SubmitOutcome::Denied as i32);
    let (_, landed) = fixture
        .call(
            Some("alice"),
            "/v1/review/submit",
            &submit(1, p::SubmitMethod::Squash, b"q"),
        )
        .await;
    assert_eq!(
        landed["outcome"],
        p::SubmitOutcome::Landed as i32,
        "{landed}"
    );
    let ids = landed["landed"].as_array().unwrap();
    assert_eq!(ids.len(), 1);
    let (_, cs) = fixture
        .call(
            Some("alice"),
            "/v1/changeset",
            &p::ChangesetRequest {
                repo: b"app".to_vec(),
                id: bytes(&ids[0]),
            },
        )
        .await;
    assert_eq!(bytes(&cs["parents"][0]), base);
    assert_eq!(cs["changes"].as_array().unwrap().len(), 2);
    assert_eq!(
        String::from_utf8(bytes(&cs["message"])).unwrap(),
        "Add files\n\nBody\n\nReview: app#1\n"
    );
}

#[tokio::test]
async fn drafts_are_private_until_published_and_restricted_accounts_see_no_reviews() {
    let fixture = fixture(POLICY);
    let base = fixture
        .commit("operator", "app", None, &[(b"a", b"1")])
        .await;
    let first = change(&fixture, "alice", base, b"b", b"2").await;
    create(&fixture, "alice", vec![first]).await;
    let (status, draft) = fixture
        .call(
            Some("bob"),
            "/v1/review/save-draft",
            &p::SaveDraftRequest {
                repo: b"app".to_vec(),
                number: 1,
                revision: 1,
                anchor: Some(p::Anchor {
                    path: b"b".to_vec(),
                    side: 0,
                    line: 1,
                    changeset_index: None,
                }),
                body: b"nit".to_vec(),
                parent: None,
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    let comments = |token: &'static str| {
        let fixture = &fixture;
        async move {
            fixture
                .call(
                    Some(token),
                    "/v1/review/list-comments",
                    &p::ListCommentsRequest {
                        repo: b"app".to_vec(),
                        number: 1,
                    },
                )
                .await
        }
    };
    assert_eq!(
        comments("bob").await.1["comments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        comments("alice").await.1["comments"]
            .as_array()
            .is_none_or(|c| c.is_empty())
    );
    let (status, published) = fixture
        .call(
            Some("bob"),
            "/v1/review/publish",
            &p::PublishRequest {
                repo: b"app".to_vec(),
                number: 1,
                revision: 1,
                vote: None,
                body: b"overall".to_vec(),
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{published}");
    let visible = comments("alice").await.1;
    assert_eq!(visible["comments"].as_array().unwrap().len(), 2);
    assert_eq!(visible["comments"][0]["draft"], false);
    // Only the author edits; anyone resolves a thread root.
    let (status, _) = fixture
        .call(
            Some("alice"),
            "/v1/review/edit-comment",
            &p::EditCommentRequest {
                repo: b"app".to_vec(),
                number: 1,
                id: 1,
                body: b"x".to_vec(),
                delete: false,
            },
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, resolved) = fixture
        .call(
            Some("alice"),
            "/v1/review/resolve-thread",
            &p::ResolveThreadRequest {
                repo: b"app".to_vec(),
                number: 1,
                id: 1,
                resolved: true,
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resolved["resolved"], true);
    for path in ["/v1/review/get", "/v1/review/submit-status"] {
        let (status, _) = fixture
            .call(
                Some("dave"),
                path,
                &p::GetReviewRequest {
                    repo: b"app".to_vec(),
                    number: 1,
                },
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, list) = fixture
        .call(
            Some("carol"),
            "/v1/review/list",
            &p::ListReviewsRequest {
                repo: b"app".to_vec(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["reviews"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn read_only_repositories_refuse_review_writes() {
    let fixture = fixture(POLICY);
    let base = fixture
        .commit("operator", "app", None, &[(b"a", b"1")])
        .await;
    let first = change(&fixture, "alice", base, b"b", b"2").await;
    create(&fixture, "alice", vec![first]).await;
    fixture.update("app", |r| r.state = kasu_core::RepositoryState::ReadOnly);
    let (status, _) = fixture
        .call(
            Some("bob"),
            "/v1/review/publish",
            &p::PublishRequest {
                repo: b"app".to_vec(),
                number: 1,
                revision: 1,
                vote: None,
                body: b"x".to_vec(),
            },
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn git_push_to_a_protected_ref_reports_review_required() {
    use tower::ServiceExt;
    let fixture = fixture(
        r#"{"review":{"rules":[{"ref_patterns":["refs/heads/release/*"]}],"bypass_accounts":["bot"]}}"#,
    );
    fixture
        .commit("operator", "app", None, &[(b"a", b"1")])
        .await;
    let (status, advertised) = fixture
        .get(Some("bob"), "/app.git/info/refs?service=git-receive-pack")
        .await;
    assert_eq!(status, StatusCode::OK);
    let text = advertisement(&advertised);
    let head = text
        .lines()
        .find(|l| l.contains("refs/heads/main"))
        .and_then(|l| l.split_whitespace().next())
        .map(|w| w[w.len() - 40..].to_owned())
        .unwrap();
    let push = |token: &'static str, name: &'static str| {
        let mut body = Vec::new();
        kasu_git::write_packet(
            &mut body,
            format!("{} {head} {name}\0report-status\n", "0".repeat(40)).as_bytes(),
        )
        .unwrap();
        body.extend(b"0000");
        let router = http_router(fixture.state.clone());
        async move {
            let response = router
                .oneshot(
                    axum::http::Request::post("/app.git/git-receive-pack")
                        .header("content-type", "application/x-git-receive-pack-request")
                        .header("authorization", format!("Bearer {token}"))
                        .body(axum::body::Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            String::from_utf8_lossy(
                &axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap(),
            )
            .into_owned()
        }
    };
    let denied = push("bob", "refs/heads/release/1").await;
    assert!(
        denied.contains("ng refs/heads/release/1 review required"),
        "{denied}"
    );
    let allowed = push("bob", "refs/heads/topic").await;
    assert!(allowed.contains("ok refs/heads/topic"), "{allowed}");
    let bypass = push("bot", "refs/heads/release/2").await;
    assert!(bypass.contains("ok refs/heads/release/2"), "{bypass}");
}

#[tokio::test]
async fn path_owners_must_approve_changes_below_their_paths() {
    let fixture = fixture(
        r#"{"review":{"rules":[{"ref_patterns":["refs/heads/main"],"required_approvals":1,"owners":[{"paths":["deploy/"],"approvers":["bot"]}]}]}}"#,
    );
    let base = fixture
        .commit("operator", "app", None, &[(b"a", b"1")])
        .await;
    let docs = change(&fixture, "alice", base.clone(), b"docs/x", b"2").await;
    create(&fixture, "alice", vec![docs]).await;
    vote(&fixture, "bob", 1, p::VoteValue::Approve).await;
    // Nothing below deploy/ changes: the owners are not needed.
    assert_eq!(submit_status(&fixture, "alice").await["ready"], true);
    let deploy = change(&fixture, "alice", base, b"deploy/y", b"3").await;
    let (status, review) = fixture
        .call(
            Some("alice"),
            "/v1/review/create",
            &p::CreateReviewRequest {
                repo: b"app".to_vec(),
                target: b"refs/heads/main".to_vec(),
                changesets: vec![deploy],
                title: b"deploy".to_vec(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{review}");
    let number = review["number"].as_u64().unwrap();
    let publish = |token: &'static str| {
        let fixture = &fixture;
        async move {
            fixture
                .call(
                    Some(token),
                    "/v1/review/publish",
                    &p::PublishRequest {
                        repo: b"app".to_vec(),
                        number,
                        revision: 1,
                        vote: Some(p::VoteValue::Approve as i32),
                        body: vec![],
                    },
                )
                .await
        }
    };
    let status_of = || {
        let fixture = &fixture;
        async move {
            fixture
                .call(
                    Some("alice"),
                    "/v1/review/submit-status",
                    &p::GetReviewRequest {
                        repo: b"app".to_vec(),
                        number,
                    },
                )
                .await
                .1
        }
    };
    publish("bob").await;
    let pending = status_of().await;
    assert_eq!(pending["ready"], false);
    assert!(
        pending["unmet"].to_string().contains("owners of deploy/"),
        "{pending}"
    );
    publish("bot").await;
    assert_eq!(status_of().await["ready"], true);
}
