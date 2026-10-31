use kasu_core::*;
use kasu_derived::*;
use kasu_git::*;
use kasu_ops::*;
use kasu_store::*;
use std::{
    collections::BTreeSet,
    io::Write,
    process::{Command, Stdio},
};
fn change(path: &[u8], blob: BlobId) -> FileChange {
    FileChange::Set {
        path: RepoPath::new(path).unwrap(),
        entry: Entry::File {
            blob,
            executable: false,
        },
    }
}
fn commit(
    ops: &Operations<'_>,
    parents: Vec<ChangesetId>,
    changes: Vec<FileChange>,
    n: u8,
) -> ChangesetId {
    let s = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: n as i64,
        tz: b"+0000".as_slice().into(),
    };
    ops.create_changeset(&Changeset {
        parents,
        author: s.clone(),
        committer: s,
        message: format!("commit {n}\n").into(),
        changes,
        git_provenance: None,
    })
    .unwrap()
}
#[test]
fn pack_sets_include_intermediate_blobs_and_second_parents_and_respect_client_shallow() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path().join("objects")).unwrap();
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
    let a = ops.ctx.repo.put_blob(b"base\n").unwrap();
    let b = ops.ctx.repo.put_blob(b"intermediate-only\n").unwrap();
    let c = ops.ctx.repo.put_blob(b"second-parent\n").unwrap();
    let root = commit(&ops, vec![], vec![change(b"a", a)], 0);
    let middle = commit(&ops, vec![root], vec![change(b"a", b)], 1);
    let restored = commit(&ops, vec![middle], vec![change(b"a", a)], 2);
    let side = commit(&ops, vec![root], vec![change(b"b", c)], 3);
    let merge = commit(&ops, vec![restored, side], vec![change(b"b", c)], 4);
    let projection = ProjectionId::full(b"repo".as_slice());
    let oids = [root, middle, restored, side, merge].map(|id| {
        engine
            .derive::<GitCommit>(&ops.ctx, id, &projection)
            .unwrap()
            .0
    });
    let index = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection,
    };
    let refs = vec![ResolvedRef {
        name: b"refs/heads/main".to_vec(),
        oid: Some(oids[4]),
        peeled: None,
        symbolic: None,
    }];
    let request = FetchRequest {
        wants: vec![oids[4]],
        done: true,
        ..Default::default()
    };
    let full = plan_fetch(&index, &refs, &request).unwrap();
    for protocol in [Protocol::V0, Protocol::V1, Protocol::V2] {
        for depth in [None, Some(1)] {
            let negotiation = plan_fetch_protocol(
                &index,
                &refs,
                &FetchRequest {
                    done: false,
                    depth,
                    ..request.clone()
                },
                protocol,
            )
            .unwrap();
            assert!(!negotiation.send_pack);
            assert!(negotiation.objects.is_empty());
            if depth.is_some() {
                assert_eq!(negotiation.shallow, BTreeSet::from([oids[4]]));
            }
        }
    }
    for protocol in [Protocol::V0, Protocol::V1] {
        let negotiation = plan_fetch_protocol(
            &index,
            &refs,
            &FetchRequest {
                done: false,
                haves: vec![oids[0]],
                ..request.clone()
            },
            protocol,
        )
        .unwrap();
        assert_eq!(negotiation.common, vec![oids[0]]);
        assert!(!negotiation.send_pack);
        assert!(negotiation.objects.is_empty());
    }
    assert!(
        full.objects
            .contains(&GitOid::hash("blob", b"intermediate-only\n"))
    );
    assert!(full.objects.contains(&oids[3]));
    assert!(
        full.objects
            .contains(&GitOid::hash("blob", b"second-parent\n"))
    );
    let filtered = plan_fetch(
        &index,
        &refs,
        &FetchRequest {
            filter: Some(FetchFilter::BlobNone),
            ..request.clone()
        },
    )
    .unwrap();
    let expected: BTreeSet<_> = full
        .objects
        .iter()
        .copied()
        .filter(|oid| index.require(*oid).unwrap().native.kind() != GitKind::Blob)
        .collect();
    assert_eq!(filtered.objects, expected);
    let promised = GitOid::hash("blob", b"base\n");
    for filter in [None, Some(FetchFilter::BlobNone)] {
        let lazy = plan_fetch(
            &index,
            &refs,
            &FetchRequest {
                wants: vec![promised],
                haves: vec![oids[4]],
                filter,
                done: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(lazy.objects, BTreeSet::from([promised]));
    }
    let secret = index.repo.put_blob(b"unpublished").unwrap();
    let secret_oid = GitOid::hash("blob", b"unpublished");
    index
        .put(secret_oid, NativeTarget::Blob(secret), b"unpublished")
        .unwrap();
    assert!(matches!(
        plan_fetch(
            &index,
            &refs,
            &FetchRequest {
                wants: vec![secret_oid],
                filter: Some(FetchFilter::BlobNone),
                done: true,
                ..Default::default()
            }
        ),
        Err(Error::Denied(_))
    ));
    let review = commit(&ops, vec![], vec![change(b"secret", secret)], 5);
    let review_oid = engine
        .derive::<GitCommit>(&ops.ctx, review, &index.projection)
        .unwrap()
        .0;
    let review_request = FetchRequest {
        wants: vec![review_oid],
        filter: Some(FetchFilter::BlobNone),
        done: true,
        ..Default::default()
    };
    assert!(matches!(
        plan_fetch(&index, &[], &review_request),
        Err(Error::Denied(_))
    ));
    index.publish_commit(review_oid).unwrap();
    let review_plan = plan_fetch(&index, &[], &review_request).unwrap();
    assert!(review_plan.objects.contains(&review_oid));
    assert!(!review_plan.objects.contains(&secret_oid));
    let promised_review_blob = FetchRequest {
        wants: vec![secret_oid],
        ..review_request
    };
    assert_eq!(
        plan_fetch(&index, &[], &promised_review_blob)
            .unwrap()
            .objects,
        BTreeSet::from([secret_oid])
    );
    let other = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: ProjectionId::full(b"other".as_slice()),
    };
    engine
        .derive::<GitCommit>(&ops.ctx, review, &other.projection)
        .unwrap();
    assert!(matches!(
        plan_fetch(&other, &[], &promised_review_blob),
        Err(Error::Denied(_))
    ));
    let shallow = plan_fetch(
        &index,
        &refs,
        &FetchRequest {
            depth: Some(1),
            ..request.clone()
        },
    )
    .unwrap();
    assert_eq!(shallow.shallow, BTreeSet::from([oids[4]]));
    assert!(!shallow.objects.contains(&oids[3]));
    assert!(
        !shallow
            .objects
            .contains(&GitOid::hash("blob", b"intermediate-only\n"))
    );
    let incremental = plan_fetch(
        &index,
        &refs,
        &FetchRequest {
            haves: vec![oids[0]],
            ..request.clone()
        },
    )
    .unwrap();
    assert!(!incremental.objects.contains(&oids[0]));
    assert!(
        !incremental
            .objects
            .contains(&GitOid::hash("blob", b"base\n"))
    );
    assert!(
        incremental
            .objects
            .contains(&GitOid::hash("blob", b"intermediate-only\n"))
    );
    let client_shallow = plan_fetch(
        &index,
        &refs,
        &FetchRequest {
            haves: vec![oids[2]],
            shallow: BTreeSet::from([oids[2]]),
            ..request
        },
    )
    .unwrap();
    assert!(
        client_shallow.objects.contains(&oids[0]),
        "shallow have does not imply possession of its ancestors"
    );
    assert!(!client_shallow.objects.contains(&oids[1]));
    let git_dir = dir.path().join("git");
    std::fs::create_dir_all(&git_dir).unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&git_dir)
            .args(["init", "--bare", "--quiet"])
            .status()
            .unwrap()
            .success()
    );
    for objects in [
        reachable(&index, [oids[0]], &BTreeSet::new()).unwrap(),
        incremental.objects,
    ] {
        let mut bytes = Vec::new();
        write_pack(&index, objects, &mut bytes).unwrap();
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&git_dir)
            .args(["index-pack", "--stdin", "--strict"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&bytes).unwrap();
        assert!(child.wait().unwrap().success());
    }
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&git_dir)
            .args(["update-ref", "refs/heads/main", &oids[4].to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&git_dir)
            .args(["fsck", "--strict"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}
#[test]
fn zlib_cache_is_bounded_and_reuses_valid_objects() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let index = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: ProjectionId::full(b"repo".as_slice()),
    };
    let cache = PackCache::new(1000);
    let mut last = None;
    for n in 0..20u8 {
        let raw = vec![n; 300];
        let id = index.repo.put_blob(&raw).unwrap();
        let oid = GitOid::hash("blob", &raw);
        index.put(oid, NativeTarget::Blob(id), &raw).unwrap();
        write_pack_cached(&index, [oid], Some(&cache), Vec::new()).unwrap();
        assert!(cache.stats().bytes <= 1000);
        last = Some(oid);
    }
    let misses = cache.stats().misses;
    let mut first = Vec::new();
    let mut second = Vec::new();
    write_pack_cached(&index, last, Some(&cache), &mut first).unwrap();
    write_pack_cached(&index, last, Some(&cache), &mut second).unwrap();
    assert_eq!(first, second);
    assert_eq!(cache.stats().misses, misses);
    assert!(cache.stats().hits >= 2);
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(8))]
    #[test]
    fn cached_pack_sets_match_graph_walk_on_branching_merge_histories(
        choices in proptest::collection::vec((proptest::num::u8::ANY, proptest::num::u8::ANY, proptest::num::u8::ANY), 48..64)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let meta = SqliteStore::memory().unwrap();
        let blobs = FsBlobStore::open(dir.path()).unwrap();
        let mut engine = Engine::standard(); register_git(&mut engine);
        let ops = Operations { engine: &engine, ctx: Ctx { repo: Repository { meta: &meta, blobs: &blobs } } };
        let pool: Vec<_> = (0..12).map(|n| ops.ctx.repo.put_blob(format!("content {n}\n").as_bytes()).unwrap()).collect();
        let projection = ProjectionId::full(b"dag".as_slice());
        let mut commits = Vec::new();
        let mut oids = Vec::new();
        for (n, (a, b, content)) in choices.iter().copied().enumerate() {
            let mut parents = Vec::new();
            if n > 0 {
                parents.push(commits[a as usize % n]);
                let second = commits[b as usize % n];
                if b % 3 == 0 && !parents.contains(&second) { parents.push(second); }
            }
            let path = format!("dir{}/file{}", content % 7, a % 3);
            let id = commit(&ops, parents, vec![change(path.as_bytes(), pool[content as usize % pool.len()])], n as u8);
            commits.push(id);
            oids.push(engine.derive::<GitCommit>(&ops.ctx, id, &projection).unwrap().0);
        }
        let index = GitIndex { repo: Repository { meta: &meta, blobs: &blobs }, projection };
        let refs: Vec<_> = oids.iter().enumerate().map(|(n, oid)| ResolvedRef { name: format!("refs/heads/b{n}").into_bytes(), oid: Some(*oid), peeled: None, symbolic: None }).collect();
        for n in [oids.len()-1, oids.len()/2, 2] {
            let wants = vec![oids[n], oids[n-1]];
            let haves = vec![oids[n/2], oids[n/3]];
            let wanted = reachable(&index, wants.iter().copied(), &BTreeSet::new()).unwrap();
            let have = reachable(&index, haves.iter().copied(), &BTreeSet::new()).unwrap();
            let mut expected: BTreeSet<_> = wanted.difference(&have).copied().collect();
            expected.extend(wants.iter().copied());
            for filter in [None, Some(FetchFilter::BlobNone)] {
                let request = FetchRequest { wants: wants.clone(), haves: haves.clone(), filter, done: true, ..Default::default() };
                let expected: BTreeSet<_> = expected.iter().copied().filter(|oid| filter.is_none() || index.require(*oid).unwrap().native.kind()!=GitKind::Blob).collect();
                for _ in 0..2 {
                    let actual = plan_fetch(&index, &refs, &request).unwrap();
                    assert_eq!(actual.objects, expected);
                    assert!(actual.shallow.is_empty() && actual.unshallow.is_empty());
                }
            }
        }
    }
}

struct CountedBlobs<'a> {
    inner: &'a dyn BlobStore,
    reads: std::sync::atomic::AtomicUsize,
}
impl BlobStore for CountedBlobs<'_> {
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.get_compressed(id)
    }
    fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        self.inner.put_compressed(id, bytes)
    }
}

#[test]
fn persistent_incremental_index_reads_only_changed_merkle_paths_and_no_warm_payloads() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join("meta.db");
    let meta = SqliteStore::open(&meta_path).unwrap();
    let fs = FsBlobStore::open(dir.path().join("objects")).unwrap();
    let blobs = CountedBlobs {
        inner: &fs,
        reads: AtomicUsize::new(0),
    };
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
    let mut changes = Vec::new();
    for d in 0..64 {
        for f in 0..8 {
            let name = format!("dir{d:02}/file{f}");
            changes.push(change(
                name.as_bytes(),
                ops.ctx.repo.put_blob(name.as_bytes()).unwrap(),
            ));
        }
    }
    let mut head = commit(&ops, vec![], changes, 0);
    for n in 1..=128u8 {
        let blob = ops
            .ctx
            .repo
            .put_blob(format!("revision {n}").as_bytes())
            .unwrap();
        head = commit(&ops, vec![head], vec![change(b"dir00/file0", blob)], n);
    }
    let projection = ProjectionId::full(b"scale".as_slice());
    let have = engine
        .derive::<GitCommit>(&ops.ctx, head, &projection)
        .unwrap()
        .0;
    let index = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: projection.clone(),
    };
    let advertised = |oid| ResolvedRef {
        name: b"refs/heads/main".to_vec(),
        oid: Some(oid),
        peeled: None,
        symbolic: None,
    };
    let initial = plan_fetch(
        &index,
        &[advertised(have)],
        &FetchRequest {
            wants: vec![have],
            done: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(initial.objects.len() > 1000);
    let blob = ops.ctx.repo.put_blob(b"new revision").unwrap();
    let next = commit(&ops, vec![head], vec![change(b"dir00/file0", blob)], 129);
    let want = engine
        .derive::<GitCommit>(&ops.ctx, next, &projection)
        .unwrap()
        .0;
    let request = FetchRequest {
        wants: vec![want],
        haves: vec![have],
        done: true,
        ..Default::default()
    };
    blobs.reads.store(0, Ordering::Relaxed);
    let cold = plan_fetch(&index, &[advertised(want)], &request).unwrap();
    assert_eq!(
        cold.objects.len(),
        4,
        "commit, root tree, changed directory and blob"
    );
    assert_eq!(
        blobs.reads.load(Ordering::Relaxed),
        3,
        "only the new commit and two changed trees are read"
    );
    // A fresh connection and a new index instance must use the durable roots.
    let reopened = SqliteStore::open(&meta_path).unwrap();
    let index = GitIndex {
        repo: Repository {
            meta: &reopened,
            blobs: &blobs,
        },
        projection,
    };
    blobs.reads.store(0, Ordering::Relaxed);
    assert_eq!(
        plan_fetch(&index, &[advertised(want)], &request)
            .unwrap()
            .objects,
        cold.objects
    );
    assert_eq!(
        blobs.reads.load(Ordering::Relaxed),
        0,
        "warm enumeration never reads Git payloads"
    );
}
