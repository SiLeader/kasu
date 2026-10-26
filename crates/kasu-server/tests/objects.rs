use kasu_core::*;
use kasu_derived::{Ctx, Engine};
use kasu_ops::Operations;
use kasu_proto::v1::{self as p, objects_client::ObjectsClient, objects_server::Objects};
use kasu_server::*;
use kasu_store::*;
use std::sync::{Arc, Mutex};
use tokio_stream::StreamExt;
use tonic::{Code, Request};

struct RecordingStore {
    fs: FsBlobStore,
    reads: Mutex<Vec<ObjectId>>,
}
impl BlobStore for RecordingStore {
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.reads.lock().unwrap().push(id);
        self.fs.get_compressed(id)
    }
    fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        self.fs.put_compressed(id, bytes)
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    meta: Arc<SqliteStore>,
    blobs: Arc<RecordingStore>,
    backend: Arc<Backend>,
    root: TreeId,
    app: TreeId,
    blob: BlobId,
    foreign_tree: TreeId,
    foreign_blob: BlobId,
}
fn request(ids: Vec<Vec<u8>>) -> Request<p::ObjectRequest> {
    let mut q = Request::new(p::ObjectRequest {
        repo: b"repo".to_vec(),
        ids,
    });
    q.metadata_mut()
        .insert("authorization", "Bearer token".parse().unwrap());
    q
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let meta = Arc::new(SqliteStore::open(temp.path().join("meta.db")).unwrap());
        let blobs = Arc::new(RecordingStore {
            fs: FsBlobStore::open(temp.path().join("objects")).unwrap(),
            reads: Mutex::new(vec![]),
        });
        let repo = Repository {
            meta: meta.as_ref(),
            blobs: blobs.as_ref(),
        };
        let mut state = 42u64;
        let bytes: Vec<_> = (0..512 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let blob = repo.put_blob(&bytes).unwrap();
        let hidden = repo.put_blob(b"outside sparse profile").unwrap();
        let foreign_blob = repo.put_blob(b"other repo only").unwrap();
        repo.associate_blob(b"other", foreign_blob).unwrap();
        let foreign_tree = repo
            .put_tree(&Tree {
                entries: [(
                    b"secret".as_slice().into(),
                    TreeEntry::Leaf(Entry::File {
                        blob: foreign_blob,
                        executable: false,
                    }),
                )]
                .into(),
            })
            .unwrap();
        repo.associate_tree_graph(b"other", foreign_tree).unwrap();
        let engine = Engine::standard();
        let ops = Operations {
            engine: &engine,
            ctx: Ctx { repo },
        };
        let sig = Signature {
            name: b"A".as_slice().into(),
            email: b"a@b".as_slice().into(),
            timestamp: 1700000000,
            tz: b"+0000".as_slice().into(),
        };
        let cs = ops
            .create_changeset(&Changeset {
                parents: vec![],
                author: sig.clone(),
                committer: sig,
                message: b"fixture".as_slice().into(),
                git_provenance: None,
                changes: [
                    (b"app/file".as_slice(), blob),
                    (b"hidden/deep/file".as_slice(), hidden),
                ]
                .into_iter()
                .map(|(path, blob)| FileChange::Set {
                    path: RepoPath::new(path).unwrap(),
                    entry: Entry::File {
                        blob,
                        executable: false,
                    },
                })
                .collect(),
            })
            .unwrap();
        ops.ctx.repo.associate_changeset(b"repo", cs).unwrap();
        let backend = Arc::new(Backend::new(
            b"repo".to_vec(),
            meta.clone(),
            blobs.clone(),
            meta.clone(),
            Some("token".into()),
        ));
        let root = TreeId(
            backend
                .get_root_tree(p::ChangesetRequest {
                    repo: b"repo".to_vec(),
                    id: cs.0.to_vec(),
                })
                .unwrap()
                .id
                .try_into()
                .unwrap(),
        );
        let TreeEntry::SubTree(app) = ops.ctx.repo.tree(root).unwrap().entries[b"app".as_slice()]
        else {
            panic!()
        };
        blobs.reads.lock().unwrap().clear();
        Self {
            _temp: temp,
            meta,
            blobs,
            backend,
            root,
            app,
            blob,
            foreign_tree,
            foreign_blob,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grpc_sparse_requests_transfer_exact_stored_frames_in_bounded_chunks() {
    let f = Fixture::new();
    let bind = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = bind.local_addr().unwrap();
    let service = ObjectService::new(f.backend.clone()).into_server();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(bind))
            .await
            .unwrap();
    });
    let mut client = ObjectsClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    for id in [f.root, f.app] {
        let mut stream = client
            .get_trees(request(vec![id.0.to_vec()]))
            .await
            .unwrap()
            .into_inner();
        let mut bytes = vec![];
        let mut last = false;
        while let Some(chunk) = stream.message().await.unwrap() {
            assert!(!last);
            assert_eq!(chunk.id, id.0);
            assert_eq!(chunk.offset as usize, bytes.len());
            assert!(chunk.data.len() <= OBJECT_CHUNK_BYTES);
            bytes.extend(chunk.data);
            last = chunk.last;
        }
        assert!(last);
        assert_eq!(bytes, f.blobs.fs.get_compressed(ObjectId(id.0)).unwrap());
        assert_eq!(Tree::decode(&decompress(&bytes).unwrap()).unwrap().id(), id);
    }
    let mut stream = client
        .get_blobs(request(vec![f.blob.0.to_vec()]))
        .await
        .unwrap()
        .into_inner();
    let mut bytes = vec![];
    let mut last = false;
    let mut count = 0;
    while let Some(chunk) = stream.message().await.unwrap() {
        assert!(!last);
        assert_eq!(chunk.id, f.blob.0);
        assert_eq!(chunk.offset as usize, bytes.len());
        assert!(chunk.data.len() <= OBJECT_CHUNK_BYTES);
        bytes.extend(chunk.data);
        last = chunk.last;
        count += 1;
    }
    assert!(last && count > 2);
    assert_eq!(
        bytes,
        f.blobs.fs.get_compressed(ObjectId(f.blob.0)).unwrap()
    );
    assert_eq!(ObjectId::hash(&decompress(&bytes).unwrap()).0, f.blob.0);
    assert_eq!(
        *f.blobs.reads.lock().unwrap(),
        vec![ObjectId(f.root.0), ObjectId(f.app.0), ObjectId(f.blob.0)]
    );
    server.abort();
}

#[tokio::test]
async fn authorization_precedes_validation_and_batches_do_not_leak_foreign_objects() {
    let f = Fixture::new();
    let service = ObjectService::new(f.backend.clone());
    let bad = Request::new(p::ObjectRequest {
        repo: b"repo".to_vec(),
        ids: vec![vec![]; 129],
    });
    assert_eq!(
        service.get_trees(bad).await.err().unwrap().code(),
        Code::Unauthenticated
    );
    assert_eq!(
        service
            .get_trees(request(vec![f.root.0.to_vec(), f.foreign_tree.0.to_vec()]))
            .await
            .err()
            .unwrap()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        service
            .get_blobs(request(vec![f.blob.0.to_vec(), f.foreign_blob.0.to_vec()]))
            .await
            .err()
            .unwrap()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        service
            .get_blobs(request(vec![vec![0; 31]]))
            .await
            .err()
            .unwrap()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        service
            .get_trees(request(vec![f.root.0.to_vec(); 2]))
            .await
            .err()
            .unwrap()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        service
            .get_trees(request(vec![vec![0; 32]; 129]))
            .await
            .err()
            .unwrap()
            .code(),
        Code::ResourceExhausted
    );
    let mut other = request(vec![f.foreign_tree.0.to_vec()]);
    other.get_mut().repo = b"other".to_vec();
    assert_eq!(
        service.get_trees(other).await.err().unwrap().code(),
        Code::PermissionDenied
    );
    assert!(f.blobs.reads.lock().unwrap().is_empty());
    assert!(
        service
            .get_trees(request(vec![]))
            .await
            .unwrap()
            .into_inner()
            .next()
            .await
            .is_none()
    );
}

#[tokio::test]
async fn cancelling_a_backpressured_stream_does_not_read_the_next_object() {
    let f = Fixture::new();
    let repo = Repository {
        meta: f.meta.as_ref(),
        blobs: f.blobs.as_ref(),
    };
    let next = repo.put_blob(b"second object").unwrap();
    repo.associate_blob(b"repo", next).unwrap();
    let service = ObjectService::new(f.backend.clone());
    let mut stream = service
        .get_blobs(request(vec![f.blob.0.to_vec(), next.0.to_vec()]))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(stream.next().await.unwrap().unwrap().offset, 0);
    drop(stream);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(*f.blobs.reads.lock().unwrap(), vec![ObjectId(f.blob.0)]);
}

#[tokio::test]
async fn empty_or_missing_stored_frame_is_an_explicit_stream_error() {
    let f = Fixture::new();
    let hex = f.blob.to_string();
    let path = f
        ._temp
        .path()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    std::fs::write(&path, []).unwrap();
    let service = ObjectService::new(f.backend.clone());
    let mut stream = service
        .get_blobs(request(vec![f.blob.0.to_vec()]))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        stream.next().await.unwrap().unwrap_err().code(),
        Code::DataLoss
    );
    // A registered member whose backing file disappeared must not silently
    // end the stream either.
    std::fs::remove_file(path).unwrap();
    let mut stream = service
        .get_blobs(request(vec![f.blob.0.to_vec()]))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        stream.next().await.unwrap().unwrap_err().code(),
        Code::NotFound
    );
}

#[tokio::test]
async fn rewrite_conflict_trees_are_available_without_a_successful_changeset() {
    let f = Fixture::new();
    let metadata = || {
        let signature = p::Signature {
            name: b"A".to_vec(),
            email: b"a@b".to_vec(),
            timestamp: 1,
            tz: b"+0000".to_vec(),
        };
        Some(p::CommitMetadata {
            author: Some(signature.clone()),
            committer: Some(signature),
            message: b"conflict fixture".to_vec(),
        })
    };
    let base = f
        .backend
        .create_changeset(p::CreateChangesetRequest {
            repo: b"repo".to_vec(),
            parents: vec![],
            changes: vec![],
            metadata: metadata(),
        })
        .unwrap();
    let branch = |path: &[u8]| {
        f.backend
            .create_changeset(p::CreateChangesetRequest {
                repo: b"repo".to_vec(),
                parents: vec![base.id.clone()],
                metadata: metadata(),
                changes: vec![p::FileChange {
                    path: path.to_vec(),
                    entry: Some(p::Entry {
                        kind: Some(p::entry::Kind::File(p::File {
                            blob: f.blob.0.to_vec(),
                            executable: false,
                        })),
                    }),
                }],
            })
            .unwrap()
    };
    let a = branch(b"conflict");
    let b = branch(b"conflict/file");
    let result = f
        .backend
        .merge(p::MergeRequest {
            repo: b"repo".to_vec(),
            a: a.id,
            b: b.id,
            metadata: metadata(),
        })
        .unwrap();
    assert!(result.id.is_none());
    assert_eq!(result.conflicts.len(), 1);
    let tree = [
        &result.conflicts[0].base,
        &result.conflicts[0].ours,
        &result.conflicts[0].theirs,
    ]
    .into_iter()
    .flatten()
    .find_map(|value| match &value.kind {
        Some(p::merge_entry::Kind::Tree(id)) => Some(id.clone()),
        _ => None,
    })
    .unwrap();
    let service = ObjectService::new(f.backend.clone());
    let mut stream = service
        .get_trees(request(vec![tree.clone()]))
        .await
        .unwrap()
        .into_inner();
    let chunk = stream.next().await.unwrap().unwrap();
    assert_eq!(chunk.id, tree);
    assert!(chunk.last);
    assert!(
        Tree::decode(&decompress(&chunk.data).unwrap())
            .unwrap()
            .entries
            .contains_key(b"file".as_slice())
    );
}
