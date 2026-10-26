use kasu_core::*;
use kasu_derived::*;
use kasu_store::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

fn entry(byte: u8) -> Entry {
    Entry::File {
        blob: BlobId([byte; 32]),
        executable: false,
    }
}
fn path(s: &str) -> RepoPath {
    RepoPath::new(s.as_bytes()).unwrap()
}
fn changes(files: &BTreeMap<String, u8>) -> Vec<FileChange> {
    files
        .iter()
        .map(|(name, byte)| FileChange::Set {
            path: path(name),
            entry: entry(*byte),
        })
        .collect()
}
fn commit(parents: Vec<ChangesetId>, message: Vec<u8>) -> Changeset {
    let sig = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: 0,
        tz: b"+0000".as_slice().into(),
    };
    Changeset {
        parents,
        author: sig.clone(),
        committer: sig,
        message: message.into(),
        changes: vec![],
        git_provenance: None,
    }
}
#[test]
fn file_directory_replacement_and_parent_sharing() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let repo = Repository {
        meta: &meta,
        blobs: &blobs,
    };
    let empty = repo.put_tree(&Tree::default()).unwrap();
    let initial = BTreeMap::from([("a/f".into(), 1), ("unchanged/f".into(), 2)]);
    let a = apply(&repo, empty, &changes(&initial)).unwrap();
    let b = apply(
        &repo,
        a,
        &[FileChange::Set {
            path: path("a/f"),
            entry: entry(3),
        }],
    )
    .unwrap();
    assert_eq!(
        lookup(&repo, a, &path("unchanged")).unwrap(),
        lookup(&repo, b, &path("unchanged")).unwrap()
    );
    let c = apply(
        &repo,
        a,
        &[
            FileChange::Set {
                path: path("a"),
                entry: entry(4),
            },
            FileChange::Delete { path: path("a/f") },
        ],
    )
    .unwrap();
    assert_eq!(
        lookup(&repo, c, &path("a")).unwrap(),
        Some(TreeEntry::Leaf(entry(4)))
    );
    assert_eq!(apply(&repo, c, &diff(&repo, c, a).unwrap()).unwrap(), a);
    assert!(
        apply(
            &repo,
            a,
            &[FileChange::Delete {
                path: path("missing")
            }]
        )
        .is_err()
    );
    assert!(
        apply(
            &repo,
            a,
            &[FileChange::Set {
                path: path("a"),
                entry: entry(4)
            }]
        )
        .is_err()
    );
}
#[test]
fn iterative_long_history_and_skip_pointers() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
    };
    let engine = Engine::standard();
    let mut ids = Vec::new();
    for n in 0u32..10_000 {
        let parents = ids.last().copied().into_iter().collect();
        ids.push(
            ctx.repo
                .put_changeset(&commit(parents, n.to_be_bytes().to_vec()))
                .unwrap(),
        );
    }
    let head = *ids.last().unwrap();
    assert_eq!(
        engine.derive::<Generation>(&ctx, head, &()).unwrap().0,
        10_000
    );
    assert_eq!(
        engine.derive::<RootTree>(&ctx, head, &()).unwrap().0,
        ctx.repo.put_tree(&Tree::default()).unwrap()
    );
    let skip = engine.derive::<FirstParentSkipV1>(&ctx, head, &()).unwrap();
    for (k, id) in skip.0.iter().enumerate() {
        assert_eq!(*id, ids[ids.len() - 1 - (1 << k)]);
    }
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn apply_diff_roundtrip(a in prop::collection::btree_map("[a-c]/[d-f]{1,3}",any::<u8>(),0..20), b in prop::collection::btree_map("[a-c]/[d-f]{1,3}",any::<u8>(),0..20)) {
        let dir = tempfile::tempdir().unwrap(); let meta = SqliteStore::memory().unwrap(); let blobs = FsBlobStore::open(dir.path()).unwrap(); let repo = Repository { meta: &meta, blobs: &blobs };
        let empty = repo.put_tree(&Tree::default()).unwrap();
        let base = apply(&repo,empty,&changes(&a)).unwrap(); let head = apply(&repo,empty,&changes(&b)).unwrap();
        prop_assert_eq!(apply(&repo,base,&diff(&repo,base,head).unwrap()).unwrap(),head);
        prop_assert_eq!(flatten(&repo,head).unwrap(), b.iter().map(|(p,b)| (path(p),entry(*b))).collect());
    }
}
