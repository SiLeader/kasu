//! Submit racing votes, policy changes, bypass pushes and repeated Submits.
//! Each seed fixes the operation mix and their start offsets; invariants are
//! checked on the final state and the review/ref logs.
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::*;
use kasu_store::*;
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}
fn account(name: &str) -> RepositoryAccount {
    RepositoryAccount {
        name: name.into(),
        token: name.into(),
        grants: vec![RepositoryGrant {
            repositories: vec![RepoPattern::parse("**").unwrap()],
            write_policy: Some(WritePolicy::default()),
            read_directories: None,
            admin: false,
        }],
        ..Default::default()
    }
}
struct World {
    _dir: tempfile::TempDir,
    store: Arc<SqliteStore>,
    state: Arc<Backend>,
}
impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteStore::open(dir.path().join("meta.db")).unwrap());
        let blobs = Arc::new(FsBlobStore::open(dir.path().join("objects")).unwrap());
        let mut record = RepositoryRecord::new(
            RepoKey::generate().unwrap(),
            RepoName::new("app").unwrap(),
            b"sim",
            1,
        );
        record.config = br#"{"review":{"rules":[{"ref_patterns":["refs/heads/main"],"required_approvals":1}],"bypass_accounts":["bot"]}}"#.to_vec();
        store.create_repository(&record).unwrap();
        let state = Backend::with_registry(
            store.clone(),
            blobs,
            store.clone(),
            store.clone(),
            Some("operator".into()),
        )
        .with_registry_ttl(Duration::from_millis(1))
        .with_reviews(store.clone())
        .with_repository_accounts(vec![account("alice"), account("bob"), account("bot")])
        .unwrap();
        Self {
            _dir: dir,
            store,
            state: Arc::new(state),
        }
    }
    async fn call(
        &self,
        token: &str,
        path: &str,
        body: &impl serde::Serialize,
    ) -> (StatusCode, serde_json::Value) {
        let response = http_router(self.state.clone())
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(serde_json::to_vec(body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 26).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }
    async fn change(
        &self,
        token: &str,
        parent: Option<Vec<u8>>,
        path: &str,
        content: &[u8],
    ) -> Vec<u8> {
        let (status, _) = self
            .call(
                token,
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
            email: b"s@example.com".to_vec(),
            timestamp: 1,
            tz: b"+0000".to_vec(),
        };
        let (status, cs) = self
            .call(
                token,
                "/v1/create-changeset",
                &p::CreateChangesetRequest {
                    repo: b"app".to_vec(),
                    parents: parent.into_iter().collect(),
                    changes: vec![p::FileChange {
                        path: path.as_bytes().to_vec(),
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
                        message: path.as_bytes().to_vec(),
                    }),
                },
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{cs}");
        bytes(&cs["id"])
    }
    fn main(&self) -> RefState {
        let key = self
            .store
            .resolve_repository_name(&RepoName::new("app").unwrap(), 0)
            .unwrap()
            .unwrap()
            .key;
        self.store
            .resolve(key.as_bytes(), b"refs/heads/main")
            .unwrap()
    }
    fn key(&self) -> RepoKey {
        self.store
            .resolve_repository_name(&RepoName::new("app").unwrap(), 0)
            .unwrap()
            .unwrap()
            .key
    }
}
fn bytes(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect()
}
fn vote(value: p::VoteValue) -> p::PublishRequest {
    p::PublishRequest {
        repo: b"app".to_vec(),
        number: 1,
        revision: 1,
        vote: Some(value as i32),
        body: vec![],
        comment_ids: vec![],
    }
}

async fn scenario(seed: u64) {
    let world = Arc::new(World::new());
    let base = world.change("bot", None, "base", b"0").await;
    let (status, _) = world
        .call(
            "bot",
            "/v1/update-ref",
            &p::UpdateRefRequest {
                repo: b"app".to_vec(),
                name: b"refs/heads/main".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/main".to_vec(),
                    target: None,
                    version: 0,
                }),
                target: Some(base.clone()),
                force: false,
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let change = world
        .change("alice", Some(base.clone()), "feature", b"1")
        .await;
    let (status, _) = world
        .call(
            "alice",
            "/v1/review/create",
            &p::CreateReviewRequest {
                repo: b"app".to_vec(),
                target: b"refs/heads/main".to_vec(),
                changesets: vec![change],
                title: b"t".to_vec(),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    world
        .call("bob", "/v1/review/publish", &vote(p::VoteValue::Approve))
        .await;

    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut tasks = Vec::new();
    for step in 0..6 {
        let delay = Duration::from_micros(rng.next() % 3000);
        let world = world.clone();
        let base = base.clone();
        let choice = rng.next() % 5;
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            match choice {
                0 | 1 => {
                    // Two Submits per key may race; each key lands at most once.
                    let key = format!("submit-{}", step % 2);
                    let (_, response) = world
                        .call(
                            "alice",
                            "/v1/review/submit",
                            &p::SubmitRequest {
                                repo: b"app".to_vec(),
                                number: 1,
                                expected_revision: 1,
                                method: p::SubmitMethod::Rebase as i32,
                                idempotency_key: key.into_bytes(),
                                ..Default::default()
                            },
                        )
                        .await;
                    Some(response)
                }
                2 => {
                    world
                        .call("bob", "/v1/review/publish", &vote(p::VoteValue::None))
                        .await;
                    world
                        .call("bob", "/v1/review/publish", &vote(p::VoteValue::Approve))
                        .await;
                    None
                }
                3 => {
                    // A bypass push advances the target meanwhile.
                    let tip = world.main();
                    let next = world
                        .change(
                            "bot",
                            tip.target.map(|t| t.0.to_vec()).or(Some(base)),
                            &format!("bot-{step}"),
                            b"b",
                        )
                        .await;
                    world
                        .call(
                            "bot",
                            "/v1/update-ref",
                            &p::UpdateRefRequest {
                                repo: b"app".to_vec(),
                                name: b"refs/heads/main".to_vec(),
                                expected: Some(p::Ref {
                                    name: b"refs/heads/main".to_vec(),
                                    target: tip.target.map(|t| t.0.to_vec()),
                                    version: tip.version,
                                }),
                                target: Some(next),
                                force: false,
                            },
                        )
                        .await;
                    None
                }
                _ => {
                    // A direct, unreviewed Land by a non-bypass account fails.
                    let tip = world.main();
                    let direct = world
                        .change(
                            "bob",
                            tip.target.map(|t| t.0.to_vec()),
                            &format!("bob-{step}"),
                            b"x",
                        )
                        .await;
                    let (_, landed) = world
                        .call(
                            "bob",
                            "/v1/land",
                            &p::LandRequest {
                                repo: b"app".to_vec(),
                                id: direct,
                                name: b"refs/heads/main".to_vec(),
                                idempotency_key: format!("land-{step}").into_bytes(),
                                ..Default::default()
                            },
                        )
                        .await;
                    assert_eq!(
                        landed["outcome"],
                        p::LandOutcome::Denied as i32,
                        "seed {seed}: {landed}"
                    );
                    None
                }
            }
        }));
    }
    let mut landed = Vec::new();
    for task in tasks {
        if let Some(response) = task.await.unwrap()
            && response["outcome"] == p::SubmitOutcome::Landed as i32
        {
            landed.push(response["landed"].clone());
        }
    }
    let (_, review) = world
        .call(
            "alice",
            "/v1/review/get",
            &p::GetReviewRequest {
                repo: b"app".to_vec(),
                number: 1,
            },
        )
        .await;
    let submitted = review["state"] == p::ReviewState::Submitted as i32;
    // Every successful Submit reports the same landing (replays included).
    assert!(
        landed.windows(2).all(|w| w[0] == w[1]),
        "seed {seed}: {landed:?}"
    );
    assert_eq!(
        submitted,
        !landed.is_empty(),
        "seed {seed}: state and outcomes agree"
    );
    let key = world.key();
    let log = world.store.tail(key.as_bytes(), 0, 1000).unwrap();
    let by_alice: Vec<_> = log.iter().filter(|e| e.actor == b"alice").collect();
    assert!(by_alice.len() <= 1, "seed {seed}: landed at most once");
    if submitted {
        let tip = bytes(
            review["submitted"]["landed"]
                .as_array()
                .unwrap()
                .last()
                .unwrap(),
        );
        assert_eq!(by_alice.len(), 1, "seed {seed}");
        assert_eq!(by_alice[0].new.unwrap().0.to_vec(), tip);
        // The approval in force at the submit point was an Approve.
        let events = world.store.review_log(key.as_bytes(), 0, 1000).unwrap();
        let submit = events
            .iter()
            .position(|e| String::from_utf8_lossy(&e.data).contains("\"submit\""))
            .unwrap();
        let last_vote = events[..submit]
            .iter()
            .rev()
            .map(|e| String::from_utf8_lossy(&e.data).into_owned())
            .find(|e| e.contains("\"publish\"") && e.contains("bob"))
            .unwrap();
        assert!(
            last_vote.contains("VOTE_VALUE_APPROVE"),
            "seed {seed}: {last_vote}"
        );
    } else {
        assert!(
            by_alice.is_empty(),
            "seed {seed}: no ref change without submission"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_never_lands_twice_or_without_approval_under_races() {
    for seed in 1..=24 {
        scenario(seed).await;
    }
}
