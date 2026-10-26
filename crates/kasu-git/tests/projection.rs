use kasu_core::*;
use kasu_derived::*;
use kasu_git::*;
use kasu_ops::*;
use kasu_store::*;
use proptest::prelude::*;

#[test]
fn native_projection_is_deterministic_and_sorts_directories_like_git() {
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
    let blob = ops.ctx.repo.put_blob(b"hello\n").unwrap();
    let sig = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: 1,
        tz: b"+0000".as_slice().into(),
    };
    let cs = Changeset {
        parents: vec![],
        author: sig.clone(),
        committer: sig,
        message: b"native\n".as_slice().into(),
        git_provenance: None,
        changes: vec![
            FileChange::Set {
                path: RepoPath::new(b"a.c".as_slice()).unwrap(),
                entry: Entry::File {
                    blob,
                    executable: false,
                },
            },
            FileChange::Set {
                path: RepoPath::new(b"a/f".as_slice()).unwrap(),
                entry: Entry::File {
                    blob,
                    executable: true,
                },
            },
            FileChange::Set {
                path: RepoPath::new(b"link".as_slice()).unwrap(),
                entry: Entry::Symlink { blob },
            },
            FileChange::Set {
                path: RepoPath::new(b"sub".as_slice()).unwrap(),
                entry: Entry::Gitlink {
                    commit: GitOid([1; 20]),
                },
            },
        ],
    };
    let id = ops.create_changeset(&cs).unwrap();
    let projection = ProjectionId::full(b"repo".as_slice());
    let commit = engine
        .derive::<GitCommit>(&ops.ctx, id, &projection)
        .unwrap()
        .0;
    let tree = engine
        .derive::<GitTree>(&ops.ctx, id, &projection)
        .unwrap()
        .0;
    let index = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: projection.clone(),
    };
    let raw = index.object(commit).unwrap().1;
    let expected = format!(
        "tree {tree}\nauthor A <a@b> 1 +0000\ncommitter A <a@b> 1 +0000\nkasu-changeset {id}\n\nnative\n"
    );
    assert_eq!(raw, expected.as_bytes());
    assert_eq!(
        id.to_string(),
        "2fc3542696560b36e742cb79e9bb5728f7c16c0481eb9e53e0a648ef496d3c16"
    );
    assert_eq!(tree.to_string(), "995637377076570f38eee38fcceb8187f6ab0eb1");
    assert_eq!(
        commit.to_string(),
        "6507c0e448c68e9dd8ad7024ce0875d3e9860123"
    );
    let raw_tree = index.object(tree).unwrap().1;
    let entries = parse_tree(&raw_tree).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
        vec![
            BString::from("a.c"),
            BString::from("a"),
            BString::from("link"),
            BString::from("sub")
        ]
    );
    assert_eq!(
        engine
            .derive::<GitCommit>(&ops.ctx, id, &projection)
            .unwrap()
            .0,
        commit
    );
    let other_projection = ProjectionId::full(b"other/repo".as_slice());
    let other = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: other_projection.clone(),
    };
    assert!(other.find(commit).unwrap().is_none());
    assert_eq!(
        engine
            .derive::<GitCommit>(&ops.ctx, id, &other_projection)
            .unwrap()
            .0,
        commit
    );
    assert_eq!(other.find_changeset(commit).unwrap(), Some(id));
    let mut signature = cs.author.clone();
    signature.name = b"bad\nheader".as_slice().into();
    let mut invalid = cs;
    invalid.author = signature;
    assert!(synthesize_commit(&invalid, tree, &[]).is_err());
    invalid.author.name = b"A".as_slice().into();
    invalid.message = b"NUL\0message".as_slice().into();
    assert!(synthesize_commit(&invalid, tree, &[]).is_err());
}
#[test]
fn git_refs_are_atomic_scoped_and_keep_tombstone_versions() {
    let store = SqliteStore::memory().unwrap();
    let value = GitRef::Direct {
        target: GitOid([1; 20]),
        peeled: Some(GitOid([2; 20])),
        kind: GitKind::Tag,
    };
    let update = GitRefUpdate {
        name: b"refs/tags/v1".to_vec(),
        expected: GitRefState::default(),
        new: Some(value.clone()),
    };
    assert!(matches!(
        store
            .compare_git(b"repo", std::slice::from_ref(&update), b"a", 0)
            .unwrap(),
        GitCasResult::Applied(_)
    ));
    let first = store.resolve_git(b"repo", &update.name).unwrap();
    assert_eq!(
        first,
        GitRefState {
            value: Some(value),
            version: 1
        }
    );
    let other = GitRefUpdate {
        name: b"refs/tags/other".to_vec(),
        ..update.clone()
    };
    assert!(matches!(
        store
            .compare_git(b"repo", &[other, update.clone()], b"a", 0)
            .unwrap(),
        GitCasResult::Mismatch { .. }
    ));
    assert!(
        store
            .resolve_git(b"repo", b"refs/tags/other")
            .unwrap()
            .value
            .is_none()
    );
    assert_eq!(store.tail_git(b"repo", 0, 100).unwrap().len(), 1);
    store
        .compare_git(
            b"repo",
            &[GitRefUpdate {
                expected: first,
                new: None,
                ..update.clone()
            }],
            b"a",
            1,
        )
        .unwrap();
    assert_eq!(store.resolve_git(b"repo", &update.name).unwrap().version, 2);
    assert!(matches!(
        store.compare_git(b"repo", &[update], b"a", 2).unwrap(),
        GitCasResult::Mismatch { .. }
    ));
    assert!(store.list_git(b"other", b"").unwrap().is_empty());
    assert!(
        store
            .compare_git(
                b"repo",
                &[GitRefUpdate {
                    name: b"refs/heads/main".to_vec(),
                    expected: GitRefState::default(),
                    new: None
                }],
                b"a",
                0
            )
            .is_err()
    );
}
proptest! {
    #[test]
    fn git_parsers_never_panic(raw in prop::collection::vec(any::<u8>(),0..2048)) {
        let _ = parse_tree(&raw); let _ = parse_commit(&raw); let _ = parse_tag(&raw); let _ = signature(&raw);
    }
}

#[test]
fn atomic_native_and_git_publication_rolls_back_both_models() {
    let store = SqliteStore::memory().unwrap();
    let native = RefUpdate {
        name: b"refs/heads/main".to_vec(),
        expected: RefState::default(),
        new: Some(ChangesetId([1; 32])),
    };
    let git = GitRefUpdate {
        name: b"refs/tags/v1".to_vec(),
        expected: GitRefState::default(),
        new: Some(GitRef::Direct {
            target: GitOid([2; 20]),
            peeled: None,
            kind: GitKind::Blob,
        }),
    };
    store
        .compare_git(b"repo", std::slice::from_ref(&git), b"first", 0)
        .unwrap();
    assert!(matches!(
        store
            .compare_native_and_git(
                b"repo",
                std::slice::from_ref(&native),
                std::slice::from_ref(&git),
                b"second",
                1
            )
            .unwrap(),
        RefPublicationResult::GitMismatch { .. }
    ));
    assert_eq!(
        store.resolve(b"repo", &native.name).unwrap(),
        RefState::default()
    );
    assert!(store.tail(b"repo", 0, 100).unwrap().is_empty());
    let stale_native = RefUpdate {
        expected: RefState {
            target: None,
            version: 1,
        },
        ..native.clone()
    };
    let fresh_git = GitRefUpdate {
        expected: store.resolve_git(b"repo", &git.name).unwrap(),
        new: None,
        ..git.clone()
    };
    assert!(matches!(
        store
            .compare_native_and_git(
                b"repo",
                &[stale_native],
                std::slice::from_ref(&fresh_git),
                b"second",
                1
            )
            .unwrap(),
        RefPublicationResult::NativeMismatch { .. }
    ));
    assert!(
        store
            .resolve_git(b"repo", &git.name)
            .unwrap()
            .value
            .is_some()
    );
    assert_eq!(store.tail_git(b"repo", 0, 100).unwrap().len(), 1);
    assert!(matches!(
        store
            .compare_native_and_git(
                b"repo",
                std::slice::from_ref(&native),
                &[fresh_git],
                b"second",
                2
            )
            .unwrap(),
        RefPublicationResult::Applied { .. }
    ));
    assert_eq!(
        store.resolve(b"repo", &native.name).unwrap().target,
        native.new
    );
    assert!(
        store
            .resolve_git(b"repo", &git.name)
            .unwrap()
            .value
            .is_none()
    );
    assert_eq!(store.tail(b"repo", 0, 100).unwrap()[0].time, 2);
    assert_eq!(store.tail_git(b"repo", 0, 100).unwrap()[1].time, 2);
}
