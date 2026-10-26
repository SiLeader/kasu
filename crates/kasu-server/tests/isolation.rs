use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use kasu_core::*;
use kasu_proto::v1::{self as p, objects_server::Objects};
use kasu_server::*;
use kasu_store::*;
use proptest::prelude::*;
use std::{collections::BTreeSet, sync::Arc};
use tokio_stream::StreamExt;
use tower::ServiceExt;

const CONTENTS: usize = 6;
const AUTH: Option<&str> = Some("Bearer operator");
fn content(n: usize) -> Vec<u8> {
    format!("content {n}\n").into_bytes()
}
fn blob(n: usize) -> BlobId {
    BlobId(ObjectId::hash(&content(n)).0)
}
/// Errors for foreign and absent IDs may differ only in the ID they name.
fn shape(status: &tonic::Status, id: &[u8]) -> (tonic::Code, String) {
    (
        status.code(),
        status.message().replace(&hex::encode(id), "<id>"),
    )
}
struct World {
    _dir: tempfile::TempDir,
    backend: Arc<Backend>,
    names: Vec<String>,
    /// Blobs each repository may read.
    readable: Vec<BTreeSet<usize>>,
    /// The changeset each repository's main branch points to.
    heads: Vec<Vec<u8>>,
}
async fn post(
    backend: &Arc<Backend>,
    path: &str,
    body: Vec<u8>,
    git: bool,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::post(path).header("authorization", "Bearer operator");
    request = if git {
        request
            .header("git-protocol", "version=2")
            .header("content-type", "application/x-git-upload-pack-request")
    } else {
        request.header("content-type", "application/json")
    };
    let response = http_router(backend.clone())
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 1 << 24)
            .await
            .unwrap()
            .to_vec(),
    )
}
async fn build(owners: &[BTreeSet<usize>], fork: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(FsBlobStore::open(dir.path()).unwrap());
    let backend = Arc::new(Backend::with_registry(
        store.clone(),
        blobs,
        store.clone(),
        store,
        Some("operator".into()),
    ));
    let mut world = World {
        _dir: dir,
        backend: backend.clone(),
        names: vec![],
        readable: vec![],
        heads: vec![],
    };
    for (n, owned) in owners.iter().enumerate() {
        let name = format!("team/repo-{n}");
        backend
            .create_repository(
                AUTH,
                p::CreateRepositoryRequest {
                    name: name.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        let repo = name.as_bytes().to_vec();
        let upload = p::UploadBlobsHttpRequest {
            chunks: owned
                .iter()
                .map(|item| p::BlobUpload {
                    repo: repo.clone(),
                    id: blob(*item).0.to_vec(),
                    offset: 0,
                    data: content(*item),
                    last: true,
                })
                .collect(),
        };
        let (status, _) = post(
            &backend,
            "/v1/upload-blobs",
            serde_json::to_vec(&upload).unwrap(),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let state = backend.scope(AUTH, &repo, Mode::Write).unwrap();
        let signature = p::Signature {
            name: b"A".to_vec(),
            email: b"a@b".to_vec(),
            timestamp: n as i64,
            tz: b"+0000".to_vec(),
        };
        let changeset = state
            .create_changeset(p::CreateChangesetRequest {
                repo: repo.clone(),
                parents: vec![],
                changes: owned
                    .iter()
                    .map(|item| p::FileChange {
                        path: format!("f{item}").into_bytes(),
                        entry: Some(p::Entry {
                            kind: Some(p::entry::Kind::File(p::File {
                                blob: blob(*item).0.to_vec(),
                                executable: false,
                            })),
                        }),
                    })
                    .collect(),
                metadata: Some(p::CommitMetadata {
                    author: Some(signature.clone()),
                    committer: Some(signature),
                    message: repo.clone(),
                }),
            })
            .unwrap();
        state
            .update_ref(p::UpdateRefRequest {
                repo: repo.clone(),
                name: b"refs/heads/main".to_vec(),
                expected: Some(p::Ref {
                    name: b"refs/heads/main".to_vec(),
                    target: None,
                    version: 0,
                }),
                target: Some(changeset.id.clone()),
                force: false,
            })
            .unwrap();
        world.names.push(name);
        world.heads.push(changeset.id);
        world.readable.push(owned.clone());
    }
    if fork {
        backend
            .fork_repository(
                AUTH,
                p::ForkRepositoryRequest {
                    source: world.names[0].clone(),
                    r#ref: b"refs/heads/main".to_vec(),
                    new_name: "fork/repo-0".into(),
                    idempotency_key: vec![],
                },
            )
            .unwrap();
        world.names.push("fork/repo-0".into());
        world.heads.push(world.heads[0].clone());
        world.readable.push(world.readable[0].clone());
    }
    world
}
async fn fetch(
    service: &ObjectService,
    repo: &[u8],
    id: &[u8],
) -> std::result::Result<Vec<u8>, tonic::Status> {
    let mut request = tonic::Request::new(p::ObjectRequest {
        repo: repo.to_vec(),
        ids: vec![id.to_vec()],
    });
    request
        .metadata_mut()
        .insert("authorization", "Bearer operator".parse().unwrap());
    let mut stream = service.get_blobs(request).await?.into_inner();
    let mut frame = Vec::new();
    while let Some(chunk) = stream.next().await {
        frame.extend(chunk?.data);
    }
    Ok(frame)
}
/// The Git OID that `name` advertises for refs/heads/main.
async fn advertised(world: &World, name: &str) -> GitOid {
    let response = http_router(world.backend.clone())
        .oneshot(
            Request::get(format!("/{name}.git/info/refs?service=git-upload-pack"))
                .header("authorization", "Bearer operator")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    main_oid(&body)
}
pub fn main_oid(advert: &[u8]) -> GitOid {
    kasu_git::parse_packets(advert)
        .unwrap()
        .into_iter()
        .find_map(|packet| match packet {
            kasu_git::Packet::Data(line) => {
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                let (oid, name) = line.split_at_checked(40)?;
                (name.split(|b| *b == 0).next() == Some(b" refs/heads/main"))
                    .then(|| std::str::from_utf8(oid).unwrap().parse().unwrap())
            }
            _ => None,
        })
        .unwrap()
}
fn want(oid: &str) -> Vec<u8> {
    let mut out = Vec::new();
    kasu_git::write_packet(&mut out, b"command=fetch\n").unwrap();
    out.extend(b"0001");
    kasu_git::write_packet(&mut out, format!("want {oid}\n").as_bytes()).unwrap();
    kasu_git::write_packet(&mut out, b"done\n").unwrap();
    out.extend(b"0000");
    out
}
async fn check(world: &World) {
    let absent = vec![0xab; 32];
    let objects = ObjectService::new(world.backend.clone());
    let mut oids = Vec::new();
    for name in &world.names {
        oids.push(advertised(world, name).await);
    }
    for (r, name) in world.names.iter().enumerate() {
        let state = world
            .backend
            .scope(AUTH, name.as_bytes(), Mode::Read)
            .unwrap();
        let repo = name.as_bytes().to_vec();
        let metadata = |id: &[u8]| {
            state.get_blob_metadata(p::ObjectRequest {
                repo: repo.clone(),
                ids: vec![id.to_vec()],
            })
        };
        let missing_metadata = shape(&metadata(&absent).unwrap_err(), &absent);
        let missing_object = shape(&fetch(&objects, &repo, &absent).await.unwrap_err(), &absent);
        for item in 0..CONTENTS {
            let id = blob(item).0;
            let visible = world.readable[r].contains(&item);
            match metadata(&id) {
                Ok(_) => assert!(visible, "{name} reads blob {item}"),
                Err(error) => {
                    assert!(!visible, "{name} cannot read its blob {item}: {error}");
                    assert_eq!(shape(&error, &id), missing_metadata);
                }
            }
            match fetch(&objects, &repo, &id).await {
                Ok(frame) => {
                    assert!(visible, "{name} fetches blob {item}");
                    assert_eq!(kasu_store::decompress(&frame).unwrap(), content(item));
                }
                Err(error) => {
                    assert!(!visible, "{name} cannot fetch its blob {item}: {error}");
                    assert_eq!(shape(&error, &id), missing_object);
                }
            }
        }
        let changeset = |id: &[u8]| {
            state.get_changeset(p::ChangesetRequest {
                repo: repo.clone(),
                id: id.to_vec(),
            })
        };
        let missing_changeset = shape(&changeset(&absent).unwrap_err(), &absent);
        let missing_oid = "ab".repeat(20);
        let missing_want = post(
            &world.backend,
            &format!("/{name}.git/git-upload-pack"),
            want(&missing_oid),
            true,
        )
        .await;
        for (other, head) in world.heads.iter().enumerate() {
            let own = world.heads[r] == *head;
            match changeset(head) {
                Ok(_) => assert!(own, "{name} reads changeset of repository {other}"),
                Err(error) => {
                    assert!(!own);
                    assert_eq!(shape(&error, head), missing_changeset);
                }
            }
            // Another repository's commit is unknown to this repository's Git
            // index and is refused as a want, like an OID that never existed.
            let found = state
                .find_changeset_by_git_oid(p::GitOidRequest {
                    repo: repo.clone(),
                    oid: oids[other].0.to_vec(),
                })
                .unwrap();
            assert_eq!(found.id.is_some(), own);
            if !own {
                let mut response = post(
                    &world.backend,
                    &format!("/{name}.git/git-upload-pack"),
                    want(&oids[other].to_string()),
                    true,
                )
                .await;
                response.1 = String::from_utf8_lossy(&response.1)
                    .replace(&oids[other].to_string(), &missing_oid)
                    .into_bytes();
                assert_eq!(response, missing_want);
                assert_ne!(response.0, StatusCode::OK);
            }
        }
    }
}
proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]
    #[test]
    fn repositories_expose_only_their_own_objects(
        owners in prop::collection::vec(prop::collection::btree_set(0..CONTENTS, 1..CONTENTS), 2..4),
        fork in any::<bool>(),
    ) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let world = build(&owners, fork).await;
            check(&world).await;
        });
    }
}
