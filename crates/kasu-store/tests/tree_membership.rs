use kasu_core::*;
use kasu_store::*;
use std::sync::Mutex;

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
#[test]
fn persisted_membership_skips_shared_subtrees_and_failed_roots_are_not_published() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("meta.db");
    let meta = SqliteStore::open(&path).unwrap();
    let blobs = RecordingStore {
        fs: FsBlobStore::open(temp.path().join("objects")).unwrap(),
        reads: Mutex::new(vec![]),
    };
    let repo = Repository {
        meta: &meta,
        blobs: &blobs,
    };
    let leaf = |blob| Tree {
        entries: [(
            b"file".as_slice().into(),
            TreeEntry::Leaf(Entry::File {
                blob,
                executable: false,
            }),
        )]
        .into(),
    };
    let initial_blob = repo.put_blob(b"initial").unwrap();
    let shared = repo.put_tree(&leaf(initial_blob)).unwrap();
    let root = repo
        .put_tree(&Tree {
            entries: [
                (b"app".as_slice().into(), TreeEntry::SubTree(shared)),
                (b"other".as_slice().into(), TreeEntry::SubTree(shared)),
            ]
            .into(),
        })
        .unwrap();
    repo.associate_tree_graph(b"repo", root).unwrap();
    assert!(repo.has_tree_membership(b"repo", shared).unwrap());
    assert!(repo.has_blob_membership(b"repo", initial_blob).unwrap());
    assert!(!repo.has_tree_membership(b"foreign", shared).unwrap());
    let updated_blob = repo.put_blob(b"updated").unwrap();
    let updated = repo.put_tree(&leaf(updated_blob)).unwrap();
    let next = repo
        .put_tree(&Tree {
            entries: [
                (b"app".as_slice().into(), TreeEntry::SubTree(updated)),
                (b"other".as_slice().into(), TreeEntry::SubTree(shared)),
            ]
            .into(),
        })
        .unwrap();
    blobs.reads.lock().unwrap().clear();
    repo.associate_tree_graph(b"repo", next).unwrap();
    assert_eq!(
        *blobs.reads.lock().unwrap(),
        vec![ObjectId(next.0), ObjectId(updated.0)]
    );
    let reopened = SqliteStore::open(&path).unwrap();
    let repo = Repository {
        meta: &reopened,
        blobs: &blobs,
    };
    blobs.reads.lock().unwrap().clear();
    repo.associate_tree_graph(b"repo", next).unwrap();
    assert!(blobs.reads.lock().unwrap().is_empty());
    let missing = TreeId([7; 32]);
    let broken = repo
        .put_tree(&Tree {
            entries: [(b"missing".as_slice().into(), TreeEntry::SubTree(missing))].into(),
        })
        .unwrap();
    assert!(matches!(
        repo.associate_tree_graph(b"repo", broken),
        Err(Error::NotFound(_))
    ));
    assert!(!repo.has_tree_membership(b"repo", broken).unwrap());
}
