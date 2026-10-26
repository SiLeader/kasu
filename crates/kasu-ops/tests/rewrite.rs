use kasu_core::*;
use kasu_derived::*;
use kasu_ops::*;
use kasu_store::*;

fn meta(message: &[u8]) -> CommitMetadata {
    let author = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: 1,
        tz: b"+0000".as_slice().into(),
    };
    CommitMetadata {
        author: author.clone(),
        committer: author,
        message: message.into(),
    }
}
fn create(ops: &Operations<'_>, parents: &[ChangesetId], path: &[u8], bytes: &[u8]) -> ChangesetId {
    let metadata = meta(bytes);
    ops.create_changeset(&Changeset {
        parents: parents.to_vec(),
        author: metadata.author,
        committer: metadata.committer,
        message: metadata.message,
        changes: vec![FileChange::Set {
            path: RepoPath::new(path).unwrap(),
            entry: Entry::File {
                blob: ops.ctx.repo.put_blob(bytes).unwrap(),
                executable: false,
            },
        }],
        git_provenance: None,
    })
    .unwrap()
}
fn id(result: RewriteResult) -> ChangesetId {
    match result {
        RewriteResult::Created { id, .. } => id,
        other => panic!("{other:?}"),
    }
}

#[test]
fn merge_rebase_preserve_parents_metadata_and_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let store = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(temp.path()).unwrap();
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: &store,
                blobs: &blobs,
            },
        },
    };
    let root = create(&ops, &[], b"f", b"a\nkeep\nc\n");
    let a = create(&ops, &[root], b"f", b"A\nkeep\nc\n");
    let b = create(&ops, &[root], b"f", b"a\nkeep\nC\n");
    let merged = id(ops.merge(a, b, meta(b"merge")).unwrap());
    let cs = ops.ctx.repo.changeset(merged).unwrap();
    assert_eq!(cs.parents, [a, b]);
    assert_eq!(cs.message.as_slice(), b"merge");
    assert!(cs.git_provenance.is_none());
    let path = RepoPath::new(b"f".as_slice()).unwrap();
    assert_eq!(ops.read_file(merged, &path).unwrap().1, b"A\nkeep\nC\n");
    let mut committer = meta(b"").committer;
    committer.timestamp = 77;
    let rebased = id(ops.rebase(a, b, committer.clone()).unwrap());
    let cs = ops.ctx.repo.changeset(rebased).unwrap();
    assert_eq!(cs.parents, [b]);
    assert_eq!(cs.author, ops.ctx.repo.changeset(a).unwrap().author);
    assert_eq!(cs.committer, committer);
    assert_eq!(
        ops.root_tree(rebased).unwrap(),
        ops.root_tree(merged).unwrap()
    );
    assert_eq!(id(ops.merge(root, a, meta(b"ignored")).unwrap()), a);
    let c = create(&ops, &[root], b"f", b"different\n");
    assert!(matches!(
        ops.rebase(a, c, meta(b"").committer).unwrap(),
        RewriteResult::Conflicted {
            stage: ConflictStage::Result,
            ..
        }
    ));
    // Root and merge changesets also have a defined replay: empty-tree delta,
    // and first-parent delta respectively.
    let unrelated = create(&ops, &[], b"other", b"unrelated");
    assert_eq!(
        ops.ctx
            .repo
            .changeset(id(ops
                .rebase(root, unrelated, meta(b"").committer)
                .unwrap()))
            .unwrap()
            .parents,
        [unrelated]
    );
    assert_eq!(
        ops.root_tree(id(ops.rebase(merged, a, meta(b"").committer).unwrap()))
            .unwrap(),
        ops.root_tree(merged).unwrap()
    );
}

#[test]
fn criss_cross_uses_all_bases_and_reports_conflicting_base_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let store = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(temp.path()).unwrap();
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: &store,
                blobs: &blobs,
            },
        },
    };
    let root = create(&ops, &[], b"f", b"base");
    let a = create(&ops, &[root], b"a", b"a");
    let b = create(&ops, &[root], b"b", b"b");
    let ab = id(ops.merge(a, b, meta(b"ab")).unwrap());
    let ba = id(ops.merge(b, a, meta(b"ba")).unwrap());
    let x = create(&ops, &[ab], b"x", b"x");
    let y = create(&ops, &[ba], b"y", b"y");
    let result = ops.merge(x, y, meta(b"final")).unwrap();
    let RewriteResult::Created { id, mut bases } = result else {
        panic!("{result:?}")
    };
    bases.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(bases, expected);
    let files = flatten(&ops.ctx.repo, ops.root_tree(id).unwrap()).unwrap();
    assert_eq!(files.len(), 5);
    for path in [b"a", b"b", b"f", b"x", b"y"] {
        assert!(files.contains_key(&RepoPath::new(path.as_slice()).unwrap()));
    }
    let a = create(&ops, &[root], b"f", b"ours");
    let b = create(&ops, &[root], b"f", b"theirs");
    let x = create(&ops, &[a, b], b"x", b"x");
    let y = create(&ops, &[b, a], b"y", b"y");
    assert!(matches!(
        ops.merge(x, y, meta(b"conflict")).unwrap(),
        RewriteResult::Conflicted {
            stage: ConflictStage::MergeBases,
            ..
        }
    ));
}

#[test]
fn three_best_bases_are_reduced_without_exposing_virtual_parents() {
    let temp = tempfile::tempdir().unwrap();
    let store = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(temp.path()).unwrap();
    let engine = Engine::standard();
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: &store,
                blobs: &blobs,
            },
        },
    };
    let root = create(&ops, &[], b"f", b"base");
    let a = create(&ops, &[root], b"a", b"a");
    let b = create(&ops, &[root], b"b", b"b");
    let c = create(&ops, &[root], b"c", b"c");
    let ab = id(ops.merge(a, b, meta(b"ab")).unwrap());
    let abc = id(ops.merge(ab, c, meta(b"abc")).unwrap());
    let tree = ops.root_tree(abc).unwrap();
    let mut tips = Vec::new();
    for (parents, path) in [
        (vec![a, b, c], b"x".as_slice()),
        (vec![c, b, a], b"y".as_slice()),
    ] {
        let m = meta(path);
        let mut changes = diff(&ops.ctx.repo, ops.root_tree(parents[0]).unwrap(), tree).unwrap();
        changes.push(FileChange::Set {
            path: RepoPath::new(path).unwrap(),
            entry: Entry::File {
                blob: ops.ctx.repo.put_blob(path).unwrap(),
                executable: false,
            },
        });
        changes.sort_by(|a, b| a.path().cmp(b.path()));
        tips.push(
            ops.create_changeset(&Changeset {
                parents,
                author: m.author,
                committer: m.committer,
                message: m.message,
                changes,
                git_provenance: None,
            })
            .unwrap(),
        );
    }
    let result = ops.merge(tips[0], tips[1], meta(b"final")).unwrap();
    let RewriteResult::Created { id, bases } = result else {
        panic!("{result:?}")
    };
    assert_eq!(bases.len(), 3);
    assert_eq!(ops.ctx.repo.changeset(id).unwrap().parents, tips);
    assert_eq!(
        flatten(&ops.ctx.repo, ops.root_tree(id).unwrap())
            .unwrap()
            .len(),
        6
    );
}
