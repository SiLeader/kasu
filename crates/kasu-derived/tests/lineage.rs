use kasu_core::*;
use kasu_derived::*;
use kasu_store::*;
use proptest::prelude::*;
fn set(path: &[u8], blob: BlobId) -> FileChange {
    FileChange::Set {
        path: RepoPath::new(path).unwrap(),
        entry: Entry::File {
            blob,
            executable: false,
        },
    }
}
fn delete(path: &[u8]) -> FileChange {
    FileChange::Delete {
        path: RepoPath::new(path).unwrap(),
    }
}
fn commit(
    ctx: &Ctx<'_>,
    parents: Vec<ChangesetId>,
    mut changes: Vec<FileChange>,
    n: i64,
) -> ChangesetId {
    changes.sort_by(|a, b| a.path().cmp(b.path()));
    let sig = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: n,
        tz: b"+0000".as_slice().into(),
    };
    ctx.repo
        .put_changeset(&Changeset {
            parents,
            author: sig.clone(),
            committer: sig,
            message: format!("{n}").into(),
            changes,
            git_provenance: None,
        })
        .unwrap()
}
#[test]
fn cold_rename_and_opt_in_copies_preserve_original_paths_and_line_numbers() {
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
    let text=b"one long original line\ntwo long original line\nthree long original line\nfour long original line\nfive long original line\n";
    let changed=b"one long original line\ntwo changed new line\nthree long original line\nfour long original line\nfive long original line\n";
    let a = ctx.repo.put_blob(text).unwrap();
    let b = ctx.repo.put_blob(changed).unwrap();
    let unrelated = ctx.repo.put_blob(b"unrelated").unwrap();
    let root = commit(
        &ctx,
        vec![],
        vec![set(b"old\xff", a), set(b"unrelated", unrelated)],
        0,
    );
    let rename = commit(
        &ctx,
        vec![root],
        vec![delete(b"old\xff"), set(b"new", b)],
        1,
    );
    let copy = commit(&ctx, vec![rename], vec![set(b"copy", b)], 2);
    let path = RepoPath::new(b"copy".as_slice()).unwrap();
    let ordinary = engine.derive::<Blame>(&ctx, copy, &path).unwrap();
    assert!(
        ordinary
            .lines
            .iter()
            .all(|line| line.changeset == copy && line.path == path)
    );
    let followed = engine.derive::<CopyBlame>(&ctx, copy, &path).unwrap().0;
    assert_eq!(followed.lines.len(), 5);
    for (n, line) in followed.lines.iter().enumerate() {
        assert_eq!(line.line, n as u64 + 1);
        if n == 1 {
            assert_eq!(line.changeset, rename);
            assert_eq!(line.path.as_bytes(), b"new");
        } else {
            assert_eq!(line.changeset, root);
            assert_eq!(line.path.as_bytes(), b"old\xff");
        }
    }
    let indexed = engine.derive::<PathIndex>(&ctx, copy, &b).unwrap();
    assert_eq!(
        indexed.0,
        vec![path.clone(), RepoPath::new(b"new".as_slice()).unwrap()]
    );
    assert_eq!(indexed.under(&path).count(), 1);
    assert!(
        meta.get(&DerivedRef::of::<PathIndex>(root, &unrelated).storage_key())
            .unwrap()
            .is_none()
    );
    let engine2 = Engine::standard();
    assert_eq!(
        engine2.derive::<CopyBlame>(&ctx, copy, &path).unwrap().0,
        followed
    );
}
#[test]
fn deleted_source_is_not_renamed_to_two_destinations_and_binary_copies_are_exact() {
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
    let blob = ctx.repo.put_blob(b"a\0b").unwrap();
    let root = commit(&ctx, vec![], vec![set(b"old", blob)], 0);
    let cs = commit(
        &ctx,
        vec![root],
        vec![delete(b"old"), set(b"a", blob), set(b"b", blob)],
        1,
    );
    let a = engine
        .derive::<CopyDetect>(&ctx, cs, &RepoPath::new(b"a".as_slice()).unwrap())
        .unwrap();
    let b = engine
        .derive::<CopyDetect>(&ctx, cs, &RepoPath::new(b"b".as_slice()).unwrap())
        .unwrap();
    assert!(a.0[0].rename);
    assert!(!b.0[0].rename);
    assert_eq!(a.0[0].similarity, 100);
    for id in [root, cs] {
        let indexed = engine.derive::<PathIndex>(&ctx, id, &blob).unwrap().0;
        let tree = engine.derive::<RootTree>(&ctx, id, &()).unwrap().0;
        let paths: Vec<_> = flatten(&ctx.repo, tree)
            .unwrap()
            .into_iter()
            .filter(|(_, entry)| entry.blob() == Some(blob))
            .map(|(path, _)| path)
            .collect();
        assert_eq!(indexed, paths);
    }
}
#[derive(Clone)]
struct LateCycle;
impl Canonical for LateCycle {
    fn encode_to(&self, _: &mut Encoder) {}
    fn decode_from(_: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self)
    }
}
impl DerivedData for LateCycle {
    const NAME: &'static str = "LateCycle";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::None;
    type SubKey = ();
    fn resolved_dependencies(_: &Ctx<'_>, cs: &Changeset, _: &()) -> Result<Vec<DerivedRef>> {
        Ok(vec![DerivedRef::of::<Self>(cs.id(), &())])
    }
    fn derive(_: &Ctx<'_>, _: &Changeset, _: &(), _: &[Self]) -> Result<Self> {
        panic!("cycle must be rejected before deriving")
    }
}
#[test]
fn late_dependency_cycles_fail_without_recursion() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
    };
    let mut engine = Engine::standard();
    engine.register::<LateCycle>();
    let id = commit(&ctx, vec![], vec![], 0);
    assert!(engine.derive::<LateCycle>(&ctx, id, &()).is_err());
}

#[test]
fn merge_tracks_the_second_parents_rename_and_recreation_starts_new_history() {
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
    let a = ctx
        .repo
        .put_blob(b"line one\nline two\nline three\nline four\n")
        .unwrap();
    let b = ctx
        .repo
        .put_blob(b"line one\nchanged two\nline three\nline four\n")
        .unwrap();
    let root = commit(&ctx, vec![], vec![set(b"old", a)], 0);
    let left = commit(&ctx, vec![root], vec![delete(b"old"), set(b"left", a)], 1);
    let right = commit(&ctx, vec![root], vec![delete(b"old"), set(b"right", b)], 2);
    let merged = commit(
        &ctx,
        vec![left, right],
        vec![delete(b"left"), set(b"merged", b)],
        3,
    );
    let path = RepoPath::new(b"merged".as_slice()).unwrap();
    let blame = engine.derive::<Blame>(&ctx, merged, &path).unwrap();
    assert_eq!(blame.lines[1].changeset, right);
    assert_eq!(blame.lines[1].path.as_bytes(), b"right");
    assert_eq!(blame.lines[0].changeset, root);
    assert_eq!(blame.lines[0].path.as_bytes(), b"old");
    let deleted = commit(&ctx, vec![merged], vec![delete(b"merged")], 4);
    let recreated = commit(&ctx, vec![deleted], vec![set(b"merged", a)], 5);
    assert!(
        engine
            .derive::<PathIndex>(&ctx, deleted, &a)
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        engine
            .derive::<CopyBlame>(&ctx, recreated, &path)
            .unwrap()
            .0
            .lines
            .iter()
            .all(|line| line.changeset == recreated)
    );
}

#[test]
fn a_thousand_renames_keep_the_root_origin_without_recursive_derivation() {
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
    let blob = ctx.repo.put_blob(b"unchanged original line\n").unwrap();
    let root = commit(&ctx, vec![], vec![set(b"file-0", blob)], 0);
    let mut tip = root;
    for n in 1..=1024 {
        tip = commit(
            &ctx,
            vec![tip],
            vec![
                delete(format!("file-{}", n - 1).as_bytes()),
                set(format!("file-{n}").as_bytes(), blob),
            ],
            n,
        );
    }
    let blame = engine
        .derive::<Blame>(&ctx, tip, &RepoPath::new(b"file-1024".as_slice()).unwrap())
        .unwrap();
    assert_eq!(blame.lines[0].changeset, root);
    assert_eq!(blame.lines[0].path.as_bytes(), b"file-0");
}

#[test]
fn similarity_matching_is_one_to_one_and_the_pair_budget_is_explicit() {
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
    let old = ctx
        .repo
        .put_blob(
            b"one long line\ntwo long line\nthree long line\nfour long line\nfive long line\n",
        )
        .unwrap();
    let a = ctx
        .repo
        .put_blob(
            b"one long line\nnew long line\nthree long line\nfour long line\nfive long line\n",
        )
        .unwrap();
    let b = ctx
        .repo
        .put_blob(
            b"one long line\nnew long line\nother long line\nfour long line\nfive long line\n",
        )
        .unwrap();
    let root = commit(&ctx, vec![], vec![set(b"old", old)], 0);
    let cs = commit(
        &ctx,
        vec![root],
        vec![delete(b"old"), set(b"a", a), set(b"b", b)],
        1,
    );
    assert!(
        engine
            .derive::<CopyDetect>(&ctx, cs, &RepoPath::new(b"a".as_slice()).unwrap())
            .unwrap()
            .0[0]
            .rename
    );
    assert!(
        engine
            .derive::<CopyDetect>(&ctx, cs, &RepoPath::new(b"b".as_slice()).unwrap())
            .unwrap()
            .0
            .is_empty()
    );
    let mut original = Vec::new();
    let mut changed = Vec::new();
    for n in 0..32 {
        let before = ctx
            .repo
            .put_blob(format!("original {n}\ncommon content line\n").as_bytes())
            .unwrap();
        let after = ctx
            .repo
            .put_blob(format!("modified {n}\ncommon content line\n").as_bytes())
            .unwrap();
        original.push(set(format!("old-{n:02}").as_bytes(), before));
        changed.push(delete(format!("old-{n:02}").as_bytes()));
        changed.push(set(format!("new-{n:02}").as_bytes(), after));
    }
    let root = commit(&ctx, vec![], original, 2);
    let cs = commit(&ctx, vec![root], changed, 3);
    let path = RepoPath::new(b"new-00".as_slice()).unwrap();
    assert!(
        engine
            .derive::<CopyDetect>(&ctx, cs, &path)
            .unwrap_err()
            .to_string()
            .contains("comparison limit")
    );
    assert!(
        meta.get(&DerivedRef::of::<CopyDetect>(cs, &path).storage_key())
            .unwrap()
            .is_none()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn reverse_index_matches_independently_materialized_tree(steps in prop::collection::vec((0u8..6,prop::option::of(0u8..4)),1..25)) {
        let dir=tempfile::tempdir().unwrap();let meta=SqliteStore::memory().unwrap();let blobs=FsBlobStore::open(dir.path()).unwrap();
        let ctx=Ctx{repo:Repository{meta:&meta,blobs:&blobs}};let engine=Engine::standard();
        let ids:Vec<_>=(0..4).map(|n|ctx.repo.put_blob(&[n]).unwrap()).collect();
        let mut model=std::collections::BTreeMap::new();let mut tip=None;
        for (n,(file,value)) in steps.into_iter().enumerate() {
            let path=format!("{}/file-{}",if file<3 {"dir"} else {"dir-other"},file%3);
            let changes=if let Some(value)=value {model.insert(path.clone(),value);vec![set(path.as_bytes(),ids[value as usize])]}
                else if model.remove(&path).is_some() {vec![delete(path.as_bytes())]}else{vec![]};
            tip=Some(commit(&ctx,tip.into_iter().collect(),changes,n as i64));
        }
        let tip=tip.unwrap();let tree=engine.derive::<RootTree>(&ctx,tip,&()).unwrap().0;
        let flat=flatten(&ctx.repo,tree).unwrap();
        for blob in ids {
            let expected:Vec<_>=flat.iter().filter(|(_,entry)|entry.blob()==Some(blob)).map(|(path,_)|path.clone()).collect();
            let actual=engine.derive::<PathIndex>(&ctx,tip,&blob).unwrap();prop_assert_eq!(&actual.0,&expected);
            for path in actual.under(&RepoPath::new(b"dir".as_slice()).unwrap()) {prop_assert!(path.as_bytes().starts_with(b"dir/"));}
        }
    }
}

#[test]
fn moved_and_copied_fragments_keep_byte_paths_and_survive_edits_and_second_parents() {
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
    let a = b"alpha original function with enough letters for tracking\nalpha body returns an original alpha value\n";
    let b = b"beta independent function with enough letters for tracking\nbeta body returns an original beta value\n";
    let joined = [a.as_slice(), b.as_slice()].concat();
    let root = commit(
        &ctx,
        vec![],
        vec![
            set(b"original\xff", ctx.repo.put_blob(&joined).unwrap()),
            set(b"dest", ctx.repo.put_blob(b"destination header\n").unwrap()),
        ],
        0,
    );
    let moved = commit(
        &ctx,
        vec![root],
        vec![set(
            b"original\xff",
            ctx.repo
                .put_blob(&[b.as_slice(), a.as_slice()].concat())
                .unwrap(),
        )],
        1,
    );
    let path = RepoPath::new(b"original\xff".as_slice()).unwrap();
    let normal = engine.derive::<Blame>(&ctx, moved, &path).unwrap();
    assert!(normal.lines.iter().any(|l| l.changeset == moved));
    let followed = engine.derive::<MovedBlame>(&ctx, moved, &path).unwrap().0;
    assert_eq!(
        followed.lines.iter().map(|l| l.line).collect::<Vec<_>>(),
        vec![3, 4, 1, 2]
    );
    assert!(
        followed
            .lines
            .iter()
            .all(|l| l.changeset == root && l.path == path)
    );
    let copied = [
        b"destination header\n".as_slice(),
        a.as_slice(),
        b"destination footer\n".as_slice(),
    ]
    .concat();
    let copy = commit(
        &ctx,
        vec![moved],
        vec![set(b"dest", ctx.repo.put_blob(&copied).unwrap())],
        2,
    );
    let dest = RepoPath::new(b"dest".as_slice()).unwrap();
    let ordinary = engine.derive::<MovedBlame>(&ctx, copy, &dest).unwrap().0;
    assert!(ordinary.lines[1..3].iter().all(|l| l.changeset == copy));
    let result = engine.derive::<CopyBlame>(&ctx, copy, &dest).unwrap().0;
    assert_eq!(result.lines[0].changeset, root);
    for (i, line) in result.lines[1..3].iter().enumerate() {
        assert_eq!(line.changeset, root);
        assert_eq!(line.path, path);
        assert_eq!(line.line, i as u64 + 1);
    }
    assert_eq!(result.lines[3].changeset, copy);
    let edited = [
        b"destination header\n".as_slice(),
        a.split_inclusive(|b| *b == b'\n').next().unwrap(),
        b"brand new edited body\ndestination footer\n".as_slice(),
    ]
    .concat();
    let edit = commit(
        &ctx,
        vec![copy],
        vec![set(b"dest", ctx.repo.put_blob(&edited).unwrap())],
        3,
    );
    let result = engine.derive::<CopyBlame>(&ctx, edit, &dest).unwrap().0;
    assert_eq!(result.lines[1].changeset, root);
    assert_eq!(result.lines[2].changeset, edit);
    assert_eq!(result.lines[3].changeset, copy);
    let side_text=b"second parent unique function with many original letters\nsecond parent body is preserved across the merge\n";
    let side = commit(
        &ctx,
        vec![root],
        vec![set(b"side\xfe", ctx.repo.put_blob(side_text).unwrap())],
        4,
    );
    let merged = [edited.as_slice(), side_text.as_slice()].concat();
    let merge = commit(
        &ctx,
        vec![edit, side],
        vec![set(b"dest", ctx.repo.put_blob(&merged).unwrap())],
        5,
    );
    let result = engine.derive::<CopyBlame>(&ctx, merge, &dest).unwrap().0;
    for (n, line) in result.lines[4..].iter().enumerate() {
        assert_eq!(line.changeset, side);
        assert_eq!(line.path.as_bytes(), b"side\xfe");
        assert_eq!(line.line, n as u64 + 1);
    }
    let second = Engine::standard();
    assert_eq!(
        second.derive::<CopyBlame>(&ctx, merge, &dest).unwrap().0,
        result
    );
    // Only sources that contributed lines need recursive parent Blame records.
    assert!(
        meta.get(&DerivedRef::of::<CopyBlame>(root, &dest).storage_key())
            .unwrap()
            .is_some()
    );
    assert!(
        meta.get(&DerivedRef::of::<CopyBlame>(side, &path).storage_key())
            .unwrap()
            .is_none()
    );
}

#[test]
fn fragment_limits_fail_without_publishing_partial_results_and_short_copies_stay_new() {
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
    let source = ctx.repo.put_blob(b"x\n").unwrap();
    let empty = ctx.repo.put_blob(b"header\n").unwrap();
    let root = commit(
        &ctx,
        vec![],
        vec![set(b"source", source), set(b"dest", empty)],
        0,
    );
    let path = RepoPath::new(b"dest".as_slice()).unwrap();
    let copy = commit(
        &ctx,
        vec![root],
        vec![set(b"dest", ctx.repo.put_blob(b"header\nx\n").unwrap())],
        1,
    );
    let result = engine.derive::<CopyBlame>(&ctx, copy, &path).unwrap().0;
    assert_eq!(
        result.lines[1].changeset, copy,
        "short unrelated matches must not become copies"
    );
    let mut changes = Vec::new();
    for n in 0..1001 {
        changes.push(set(format!("candidate-{n:04}").as_bytes(), source));
    }
    let broad = commit(&ctx, vec![copy], changes, 2);
    let large = commit(
        &ctx,
        vec![broad],
        vec![set(
            b"dest",
            ctx.repo
                .put_blob(b"header\nan unmatched new line with many letters\n")
                .unwrap(),
        )],
        3,
    );
    let error = engine.derive::<CopyBlame>(&ctx, large, &path).unwrap_err();
    assert!(error.to_string().contains("source limit"), "{error}");
    assert!(
        meta.get(&DerivedRef::of::<CopyBlame>(large, &path).storage_key())
            .unwrap()
            .is_none()
    );
    assert!(
        meta.get(
            &DerivedRef::of::<LineMatches>(large, &LineMatchKey { path, copies: true })
                .storage_key()
        )
        .unwrap()
        .is_none()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn reordered_unique_blocks_keep_all_original_line_numbers(order in prop::collection::vec(any::<u32>(), 8)) {
        let dir = tempfile::tempdir().unwrap();
        let meta = SqliteStore::memory().unwrap();
        let blobs = FsBlobStore::open(dir.path()).unwrap();
        let ctx = Ctx { repo: Repository { meta: &meta, blobs: &blobs } };
        let engine = Engine::standard();
        let contents: Vec<_> = (0..8).map(|n| format!("unique block {n} has more than twenty original alphabetic characters\n").into_bytes()).collect();
        let mut permutation: Vec<_> = (0..8).collect();
        permutation.sort_by_key(|n| (order[*n], *n));
        let original: Vec<_> = contents.iter().flatten().copied().collect();
        let reordered: Vec<_> = permutation.iter().flat_map(|n|contents[*n].iter()).copied().collect();
        let root = commit(&ctx,vec![],vec![set(b"f",ctx.repo.put_blob(&original).unwrap())],0);
        let moved = commit(&ctx,vec![root],vec![set(b"f",ctx.repo.put_blob(&reordered).unwrap())],1);
        let result = engine.derive::<MovedBlame>(&ctx,moved,&RepoPath::new(b"f".as_slice()).unwrap()).unwrap().0;
        assert_eq!(result.lines.iter().map(|l|l.line).collect::<Vec<_>>(),permutation.iter().map(|n|*n as u64+1).collect::<Vec<_>>());
        assert!(result.lines.iter().all(|l|l.changeset==root));
    }
}

#[test]
fn ambiguous_fragment_work_is_bounded_and_large_roots_need_no_search() {
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
    let repeated = b"repeated source line with plenty of original letters\n".repeat(1001);
    let root = commit(
        &ctx,
        vec![],
        vec![
            set(b"source", ctx.repo.put_blob(&repeated).unwrap()),
            set(b"dest", ctx.repo.put_blob(b"header\n").unwrap()),
        ],
        0,
    );
    let target = [b"header\n".as_slice(), repeated.as_slice()].concat();
    let copy = commit(
        &ctx,
        vec![root],
        vec![set(b"dest", ctx.repo.put_blob(&target).unwrap())],
        1,
    );
    let path = RepoPath::new(b"dest".as_slice()).unwrap();
    assert!(
        engine
            .derive::<CopyBlame>(&ctx, copy, &path)
            .unwrap_err()
            .to_string()
            .contains("comparison limit")
    );
    assert!(
        meta.get(&DerivedRef::of::<CopyBlame>(copy, &path).storage_key())
            .unwrap()
            .is_none()
    );
    let many_lines = ctx.repo.put_blob(&b"x\n".repeat(100_001)).unwrap();
    let standalone = commit(&ctx, vec![], vec![set(b"large", many_lines)], 2);
    let lines = engine
        .derive::<CopyBlame>(
            &ctx,
            standalone,
            &RepoPath::new(b"large".as_slice()).unwrap(),
        )
        .unwrap()
        .0
        .lines;
    assert_eq!(lines.len(), 100_001);
    assert!(lines.iter().all(|l| l.changeset == standalone));
}
