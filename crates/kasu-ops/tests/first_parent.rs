use kasu_core::*;
use kasu_derived::*;
use kasu_ops::*;
use kasu_store::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Observed<'a> {
    inner: &'a dyn MetaStore,
    reads: AtomicU64,
    old_reads: AtomicU64,
    bytes: AtomicU64,
}
impl MetaStore for Observed<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if key.starts_with(b"derived/FirstParentSkip@2/") {
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
        if key.starts_with(b"derived/FirstParentSkip@1/") {
            self.old_reads.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get(key)
    }
    fn put_if_absent(&self, key: &[u8], value: &[u8]) -> Result<()> {
        if key.starts_with(b"derived/FirstParentSkip@2/") {
            self.bytes.store(value.len() as u64, Ordering::Relaxed);
        }
        self.inner.put_if_absent(key, value)
    }
}
fn cs(parents: Vec<ChangesetId>, n: u32) -> Changeset {
    let sig = Signature {
        name: b"index".as_slice().into(),
        email: b"index@example.invalid".as_slice().into(),
        timestamp: 0,
        tz: b"+0000".as_slice().into(),
    };
    Changeset {
        parents,
        author: sig.clone(),
        committer: sig,
        message: n.to_be_bytes().as_slice().into(),
        changes: vec![],
        git_provenance: None,
    }
}
#[test]
fn eager_skip_index_has_fixed_reads_and_bytes_and_queries_skip_long_histories() {
    let dir = tempfile::tempdir().unwrap();
    let database = SqliteStore::memory().unwrap();
    let meta = Observed {
        inner: &database,
        reads: AtomicU64::new(0),
        old_reads: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    };
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
        },
    };
    let mut ids = Vec::new();
    for n in 0..10000 {
        meta.reads.store(0, Ordering::Relaxed);
        let id = ops
            .create_changeset(&cs(ids.last().copied().into_iter().collect(), n))
            .unwrap();
        assert!(
            meta.reads.load(Ordering::Relaxed) <= 7,
            "index reads at {n}"
        );
        assert_eq!(
            meta.bytes.load(Ordering::Relaxed),
            if n == 0 { 10 } else { 82 }
        );
        assert_eq!(meta.old_reads.load(Ordering::Relaxed), 0);
        ids.push(id);
    }
    let head = *ids.last().unwrap();
    // Boundary heights plus deterministic samples, compared with actual source IDs.
    for target in (0..128).chain((128..10000).step_by(37)).chain([9998, 9999]) {
        meta.reads.store(0, Ordering::Relaxed);
        assert!(ops.is_first_parent_ancestor(ids[target], head).unwrap());
        assert!(
            meta.reads.load(Ordering::Relaxed) <= 100,
            "query index reads for depth {target}"
        );
    }
    let root = ids[0];
    let other = ops.create_changeset(&cs(vec![root], 20_000)).unwrap();
    let merge = ops
        .create_changeset(&cs(vec![other, head], 20_001))
        .unwrap();
    assert!(ops.is_ancestor(head, merge).unwrap());
    assert!(!ops.is_first_parent_ancestor(head, merge).unwrap());
    assert!(!ops.is_first_parent_ancestor(ids[1], merge).unwrap());
    assert!(ops.is_first_parent_ancestor(other, merge).unwrap());
    assert_eq!(
        engine
            .derive::<FirstParentSkip>(&ops.ctx, merge, &())
            .unwrap()
            .depth,
        2
    );
}
#[test]
fn v1_records_survive_reopen_and_v2_backfill_without_changing_changesets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.db");
    let object_path = dir.path().join("objects");
    let (ids, old) = {
        let meta = SqliteStore::open(&path).unwrap();
        let blobs = FsBlobStore::open(&object_path).unwrap();
        let mut legacy = Engine::default();
        legacy.register::<RootTree>();
        legacy.register::<Generation>();
        legacy.register::<FirstParentSkipV1>();
        let ops = Operations {
            engine: &legacy,
            ctx: Ctx {
                repo: Repository {
                    meta: &meta,
                    blobs: &blobs,
                },
            },
        };
        let mut ids = Vec::new();
        for n in 0..1000 {
            ids.push(
                ops.create_changeset(&cs(ids.last().copied().into_iter().collect(), n))
                    .unwrap(),
            );
        }
        let old = legacy
            .derive::<FirstParentSkipV1>(&ops.ctx, *ids.last().unwrap(), &())
            .unwrap();
        assert!(
            meta.get(&DerivedRef::of::<FirstParentSkip>(*ids.last().unwrap(), &()).storage_key())
                .unwrap()
                .is_none()
        );
        (ids, old.encode())
    };
    let meta = SqliteStore::open(&path).unwrap();
    let blobs = FsBlobStore::open(&object_path).unwrap();
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
        },
    };
    let head = *ids.last().unwrap();
    let v2 = engine
        .derive::<FirstParentSkip>(&ops.ctx, head, &())
        .unwrap();
    assert_eq!(v2.depth, 999);
    assert_eq!(v2.parent, Some(ids[998]));
    assert_eq!(v2.encode().len(), 82);
    assert_eq!(
        meta.get(&DerivedRef::of::<FirstParentSkipV1>(head, &()).storage_key())
            .unwrap()
            .unwrap(),
        old
    );
    assert_eq!(
        engine
            .derive::<FirstParentSkipV1>(&ops.ctx, head, &())
            .unwrap()
            .encode(),
        old
    );
    assert!(ops.is_first_parent_ancestor(ids[500], head).unwrap());
    for (n, id) in ids.iter().enumerate() {
        assert_eq!(ops.ctx.repo.changeset(*id).unwrap().id(), *id);
        assert_eq!(
            engine
                .derive::<FirstParentSkip>(&ops.ctx, *id, &())
                .unwrap()
                .depth,
            n as u64
        );
    }
    let appended = ops.create_changeset(&cs(vec![head], 1001)).unwrap();
    assert_eq!(
        engine
            .derive::<FirstParentSkip>(&ops.ctx, appended, &())
            .unwrap()
            .depth,
        1000
    );
}
#[test]
fn skip_codec_rejects_incoherent_depths_and_links() {
    let root = FirstParentSkip {
        depth: 0,
        parent: None,
        jump: None,
    };
    assert_eq!(FirstParentSkip::decode(&root.encode()).unwrap(), root);
    let parent = ChangesetId([1; 32]);
    let jump = ChangesetId([2; 32]);
    let valid = FirstParentSkip {
        depth: 7,
        parent: Some(parent),
        jump: Some(FirstParentJump { id: jump, depth: 0 }),
    };
    assert_eq!(FirstParentSkip::decode(&valid.encode()).unwrap(), valid);
    for bad in [
        FirstParentSkip {
            depth: 0,
            ..valid.clone()
        },
        FirstParentSkip {
            depth: 7,
            parent: None,
            ..valid.clone()
        },
        FirstParentSkip {
            jump: Some(FirstParentJump { id: jump, depth: 7 }),
            ..valid.clone()
        },
        FirstParentSkip {
            jump: Some(FirstParentJump {
                id: parent,
                depth: 0,
            }),
            ..valid.clone()
        },
        FirstParentSkip {
            jump: Some(FirstParentJump { id: jump, depth: 5 }),
            ..valid
        },
    ] {
        assert!(FirstParentSkip::decode(&bad.encode()).is_err());
    }
}
