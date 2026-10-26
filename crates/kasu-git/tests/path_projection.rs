use kasu_core::*;
use kasu_derived::*;
use kasu_git::*;
use kasu_ops::*;
use kasu_store::*;
use std::collections::BTreeSet;

fn commit(
    ops: &Operations<'_>,
    parents: &[ChangesetId],
    changes: Vec<FileChange>,
    n: u8,
) -> ChangesetId {
    let signature = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: n.into(),
        tz: b"+0000".as_slice().into(),
    };
    ops.create_changeset(&Changeset {
        parents: parents.into(),
        author: signature.clone(),
        committer: signature,
        message: format!("change {n}\n").into(),
        changes,
        git_provenance: None,
    })
    .unwrap()
}
fn set(path: &[u8], blob: BlobId) -> FileChange {
    FileChange::Set {
        path: RepoPath::new(path).unwrap(),
        entry: Entry::File {
            blob,
            executable: false,
        },
    }
}

#[test]
fn projected_dag_skips_outside_changes_preserves_merges_and_deletion_and_is_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
        },
    };
    let a = ops.ctx.repo.put_blob(b"a").unwrap();
    let b = ops.ctx.repo.put_blob(b"b").unwrap();
    let secret = ops.ctx.repo.put_blob(b"outside secret").unwrap();
    let zero = commit(&ops, &[], vec![set(b"secret", secret)], 0);
    let first = commit(&ops, &[zero], vec![set(b"deploy/a", a)], 1);
    let outside = commit(&ops, &[first], vec![set(b"secret2", secret)], 2);
    let side = commit(&ops, &[first], vec![set(b"deploy/b", b)], 3);
    let main = commit(&ops, &[outside], vec![set(b"deploy/a", b)], 4);
    let merge = commit(&ops, &[main, side], vec![set(b"deploy/b", b)], 5);
    let unrelated = commit(&ops, &[first], vec![set(b"unrelated", secret)], 6);
    let redundant_merge = commit(&ops, &[merge, unrelated], vec![], 7);
    let deletion = commit(
        &ops,
        &[redundant_merge],
        vec![
            FileChange::Delete {
                path: RepoPath::new(b"deploy/a".as_slice()).unwrap(),
            },
            FileChange::Delete {
                path: RepoPath::new(b"deploy/b".as_slice()).unwrap(),
            },
        ],
        8,
    );
    let recreate = commit(&ops, &[deletion], vec![set(b"deploy/a", a)], 9);
    let key = ProjectionId::path(
        b"repo".as_slice(),
        RepoPath::new(b"deploy".as_slice()).unwrap(),
    );
    let derive = |id| engine.derive::<PathCommit>(&ops.ctx, id, &key).unwrap().0;
    assert_eq!(derive(zero), None);
    assert_eq!(derive(first), derive(outside));
    assert_eq!(derive(merge), derive(redundant_merge));
    let tip = derive(recreate).unwrap();
    assert_eq!(tip.to_string(), "d54ec756fb3f94f89b2d7941ceedb1bac958129e");
    let index = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: key.clone(),
    };
    let (_, raw) = index.object(derive(merge).unwrap()).unwrap();
    assert_eq!(parse_commit(&raw).unwrap().parents.len(), 2);
    let (_, raw) = index.object(derive(deletion).unwrap()).unwrap();
    assert_eq!(
        oid_from_hex(parse_commit(&raw).unwrap().tree).unwrap(),
        GitOid::hash("tree", b"")
    );
    let objects = reachable(&index, [tip], &BTreeSet::new()).unwrap();
    let sources: BTreeSet<_> = objects
        .iter()
        .filter_map(|oid| index.find_changeset(*oid).unwrap())
        .collect();
    assert_eq!(
        sources,
        BTreeSet::from([first, side, main, merge, deletion, recreate])
    );
    assert!(!objects.contains(&GitOid::hash("blob", b"outside secret")));
    assert!(
        index
            .find(GitOid::hash("blob", b"outside secret"))
            .unwrap()
            .is_none()
    );
    let full = ProjectionId::full(b"repo".as_slice());
    let full_oid = engine
        .derive::<GitCommit>(&ops.ctx, recreate, &full)
        .unwrap()
        .0;
    assert_ne!(full_oid, tip);
    assert!(index.find(full_oid).unwrap().is_none());
    let checkout = dir.path().join("projected.git");
    std::fs::create_dir(&checkout).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["init", "--bare", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    let mut pack = Vec::new();
    write_pack(&index, objects.clone(), &mut pack).unwrap();
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["index-pack", "--stdin", "--strict"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&pack).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["update-ref", "refs/heads/main", &tip.to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["fsck", "--strict"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let refs = [ResolvedRef {
        name: b"refs/heads/main".to_vec(),
        oid: Some(tip),
        peeled: None,
        symbolic: None,
    }];
    assert!(matches!(
        plan_fetch(
            &index,
            &refs,
            &FetchRequest {
                wants: vec![full_oid],
                done: true,
                ..Default::default()
            }
        ),
        Err(Error::Denied(_))
    ));
    // The immutable derived cache and OID index survive a fresh engine instance.
    let mut reopened = Engine::standard();
    register_git(&mut reopened);
    assert_eq!(
        reopened
            .derive::<PathCommit>(&ops.ctx, recreate, &key)
            .unwrap()
            .0,
        Some(tip)
    );
    let other = ProjectionId::path(
        b"repo".as_slice(),
        RepoPath::new(b"secret".as_slice()).unwrap(),
    );
    assert_eq!(
        engine
            .derive::<PathCommit>(&ops.ctx, recreate, &other)
            .unwrap()
            .0,
        None
    );
    let bytes = key.encode();
    assert_eq!(ProjectionId::decode(&bytes).unwrap(), key);
    assert_eq!(
        ProjectionId::full(b"repo".as_slice()).encode(),
        [vec![0], b"\0\0\0\x04repo".to_vec()].concat()
    );
}

#[test]
fn non_utf8_path_and_file_replacing_directory_have_deterministic_history() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let ops = Operations {
        engine: &engine,
        ctx: Ctx {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
        },
    };
    let blob = ops.ctx.repo.put_blob(b"ok").unwrap();
    let first = commit(&ops, &[], vec![set(b"\xff/a", blob)], 0);
    let file = commit(
        &ops,
        &[first],
        vec![
            set(b"\xff", blob),
            FileChange::Delete {
                path: RepoPath::new(b"\xff/a".as_slice()).unwrap(),
            },
        ],
        1,
    );
    let key = ProjectionId::path(
        b"repo".as_slice(),
        RepoPath::new(b"\xff".as_slice()).unwrap(),
    );
    let a = engine
        .derive::<PathCommit>(&ops.ctx, first, &key)
        .unwrap()
        .0
        .unwrap();
    let b = engine
        .derive::<PathCommit>(&ops.ctx, file, &key)
        .unwrap()
        .0
        .unwrap();
    assert_ne!(a, b);
    let index = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: key.clone(),
    };
    let (_, raw) = index.object(b).unwrap();
    let parsed = parse_commit(&raw).unwrap();
    assert_eq!(
        oid_from_hex(parsed.tree).unwrap(),
        GitOid::hash("tree", b"")
    );
    assert_eq!(parsed.parents.len(), 1);
    assert_eq!(oid_from_hex(parsed.parents[0]).unwrap(), a);
    assert_eq!(ProjectionId::decode(&key.encode()).unwrap(), key);
}
