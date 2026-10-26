use kasu_core::*;
use kasu_proto::v1 as p;
use kasu_server::{Backend, LandPreparation};
use kasu_store::{BlobStore, RefState, RefStore, RefUpdate, Repository, SqliteStore};
use prost::Message;
use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};
#[derive(Default)]
struct Blobs(Mutex<BTreeMap<ObjectId, Vec<u8>>>);
impl BlobStore for Blobs {
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.0
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }
    fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        let raw = kasu_store::decompress(bytes)?;
        if ObjectId::hash(&raw) != id {
            return Err(invalid("bad fixture object hash"));
        }
        let mut values = self.0.lock().unwrap();
        if let Some(old) = values.get(&id) {
            assert_eq!(kasu_store::decompress(old).unwrap(), raw);
        } else {
            values.insert(id, bytes.to_vec());
        }
        Ok(())
    }
}
fn fixture() -> (Backend, ChangesetId) {
    let meta = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(Blobs::default());
    let repo = Repository {
        meta: meta.as_ref(),
        blobs: blobs.as_ref(),
    };
    let blob = repo.put_blob(b"seed\n").unwrap();
    repo.associate_blob(b"repo", blob).unwrap();
    let sig = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: 0,
        tz: b"+0000".as_slice().into(),
    };
    let id = repo
        .put_changeset(&Changeset {
            parents: vec![],
            author: sig.clone(),
            committer: sig,
            message: b"seed".as_slice().into(),
            changes: vec![FileChange::Set {
                path: RepoPath::new(b"file".as_slice()).unwrap(),
                entry: Entry::File {
                    blob,
                    executable: false,
                },
            }],
            git_provenance: None,
        })
        .unwrap();
    repo.associate_changeset(b"repo", id).unwrap();
    meta.compare_and_swap(
        b"repo",
        &[RefUpdate {
            name: b"refs/heads/main".to_vec(),
            expected: RefState::default(),
            new: Some(id),
        }],
        b"fuzz",
        0,
    )
    .unwrap();
    (
        Backend::new(b"repo".to_vec(), meta.clone(), blobs, meta, None),
        id,
    )
}
fn observe<T: Message + Default + PartialEq + Debug>(
    result: std::result::Result<T, tonic::Status>,
    foreign: bool,
) {
    if foreign {
        assert_eq!(result.unwrap_err().code(), tonic::Code::PermissionDenied);
    } else if let Ok(response) = result {
        let raw = response.encode_to_vec();
        assert_eq!(T::decode(raw.as_slice()).unwrap(), response);
    }
}
pub fn native_api(data: &[u8]) {
    if data.len() > crate::MAX_INPUT {
        return;
    }
    let Some((&selector, raw)) = data.split_first() else {
        return;
    };
    let index = selector as usize % 17;
    let mode = selector as usize / 17;
    let repair = mode & 1 != 0;
    let foreign = mode & 2 != 0;
    macro_rules! input {
        ($name:ident) => {
            match p::$name::decode(raw) {
                Ok(q) => q,
                Err(_) => return,
            }
        };
    }
    let (backend, id) = fixture();
    let repo = if foreign {
        b"other".to_vec()
    } else {
        b"repo".to_vec()
    };
    macro_rules! send {
        ($q:ident,$method:ident) => {{
            $q.repo = repo.clone();
            observe(backend.$method($q), foreign);
        }};
    }
    match index {
        0 => {
            let mut q = input!(ChangesetRequest);
            if repair {
                q.id = id.0.to_vec();
            }
            send!(q, get_changeset);
        }
        1 => {
            let mut q = input!(ChangesetRequest);
            if repair {
                q.id = id.0.to_vec();
            }
            send!(q, get_root_tree);
        }
        2 => {
            let mut q = input!(RefRequest);
            send!(q, resolve_ref);
        }
        3 => {
            let mut q = input!(ListRefsRequest);
            send!(q, list_refs);
        }
        4 => {
            let mut q = input!(LogRequest);
            if repair {
                q.start = id.0.to_vec();
            }
            send!(q, log);
        }
        5 => {
            let mut q = input!(CompareRequest);
            if repair {
                q.base = id.0.to_vec();
                q.head = id.0.to_vec();
            }
            send!(q, is_ancestor);
        }
        6 => {
            let mut q = input!(CompareRequest);
            if repair {
                q.base = id.0.to_vec();
                q.head = id.0.to_vec();
            }
            send!(q, merge_base);
        }
        7 => {
            let mut q = input!(DiffRequest);
            if repair {
                q.base = id.0.to_vec();
                q.head = id.0.to_vec();
            }
            send!(q, diff);
        }
        8 => {
            let mut q = input!(PathRequest);
            if repair {
                q.id = id.0.to_vec();
            }
            send!(q, blame);
        }
        9 => {
            let mut q = input!(PathRequest);
            if repair {
                q.id = id.0.to_vec();
            }
            send!(q, read_file);
        }
        10 => {
            let mut q = input!(PathRequest);
            if repair {
                q.id = id.0.to_vec();
            }
            send!(q, list_dir);
        }
        11 => {
            let mut q = input!(GitOidRequest);
            send!(q, find_changeset_by_git_oid);
        }
        12 => {
            let mut q = input!(CreateChangesetRequest);
            if repair {
                for parent in &mut q.parents {
                    *parent = id.0.to_vec();
                }
            }
            send!(q, create_changeset);
        }
        13 => {
            let mut q = input!(MergeRequest);
            if repair {
                q.a = id.0.to_vec();
                q.b = id.0.to_vec();
            }
            send!(q, merge);
        }
        14 => {
            let mut q = input!(RebaseRequest);
            if repair {
                q.id = id.0.to_vec();
                q.onto = id.0.to_vec();
            }
            send!(q, rebase);
        }
        15 => {
            let mut q = input!(UpdateRefRequest);
            if repair {
                q.target = Some(id.0.to_vec());
            }
            send!(q, update_ref);
        }
        16 => {
            let mut q = input!(LandRequest);
            q.repo = repo;
            if repair {
                q.id = id.0.to_vec();
            }
            match backend.begin_land(q) {
                Err(error) => {
                    if foreign {
                        assert_eq!(error.code(), tonic::Code::PermissionDenied);
                    }
                }
                Ok(operation) => {
                    assert!(!foreign);
                    assert!(operation.max_attempts() <= 32);
                    if let Ok(LandPreparation::Ready(prepared)) =
                        backend.prepare_land(&operation, 1)
                    {
                        let _ = backend.commit_land(prepared, &|| mode & 4 != 0);
                    }
                }
            }
        }
        _ => unreachable!(),
    }
}
pub fn seed_id() -> ChangesetId {
    fixture().1
}
