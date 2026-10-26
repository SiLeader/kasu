use kasu_core::*;
use kasu_derived::apply;
use kasu_ops::*;
use kasu_store::*;
use proptest::prelude::*;

#[test]
fn text_merge_preserves_bytes_and_reports_ranges_without_markers() {
    assert_eq!(
        merge_text(b"one\nkeep\nthree", b"ONE\nkeep\nthree", b"one\nkeep\n\xff"),
        TextMerge::Clean(b"ONE\nkeep\n\xff".to_vec())
    );
    assert_eq!(
        merge_text(b"a\nb\nc\n", b"A\nb\nc\n", b"T\nb\nc\n"),
        TextMerge::Conflicted(vec![LineConflict {
            base: 0..1,
            ours: b"A\n".to_vec(),
            theirs: b"T\n".to_vec()
        }])
    );
    assert_eq!(
        merge_text(b"", b"ours", b"theirs"),
        TextMerge::Conflicted(vec![LineConflict {
            base: 0..0,
            ours: b"ours".to_vec(),
            theirs: b"theirs".to_vec()
        }])
    );
    assert_eq!(
        merge_text(b"a\0", b"b\0", b"c\0"),
        TextMerge::BinaryConflict
    );
    assert_eq!(
        merge_text(b"a\0", b"a\0", b"c\0"),
        TextMerge::Clean(b"c\0".to_vec())
    );
    // Two independent conflict regions must both be retained.
    let TextMerge::Conflicted(conflicts) =
        merge_text(b"a\ngap\nc\n", b"A\ngap\nC\n", b"X\ngap\nZ\n")
    else {
        panic!("expected conflicts")
    };
    assert_eq!(
        conflicts.iter().map(|c| c.base.clone()).collect::<Vec<_>>(),
        [0..1, 2..3]
    );
}

fn change(repo: &Repository<'_>, path: &[u8], bytes: &[u8], executable: bool) -> FileChange {
    FileChange::Set {
        path: RepoPath::new(path).unwrap(),
        entry: Entry::File {
            blob: repo.put_blob(bytes).unwrap(),
            executable,
        },
    }
}

#[test]
fn tree_merge_combines_content_modes_and_directories_and_classifies_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let repo = Repository {
        meta: &meta,
        blobs: &blobs,
    };
    let empty = repo.put_tree(&Tree::default()).unwrap();
    let base = apply(
        &repo,
        empty,
        &[change(&repo, b"dir/file", b"a\nkeep\nc\n", false)],
    )
    .unwrap();
    let ours = apply(
        &repo,
        base,
        &[
            change(&repo, b"dir/file", b"A\nkeep\nc\n", true),
            change(&repo, b"new/a", b"ours", false),
        ],
    )
    .unwrap();
    let theirs = apply(
        &repo,
        base,
        &[
            change(&repo, b"dir/file", b"a\nkeep\nC\n", false),
            change(&repo, b"new/b", b"theirs", false),
        ],
    )
    .unwrap();
    let merged = merge_trees(&repo, base, ours, theirs).unwrap();
    assert!(merged.conflicts.is_empty());
    let expected = apply(
        &repo,
        base,
        &[
            change(&repo, b"dir/file", b"A\nkeep\nC\n", true),
            change(&repo, b"new/a", b"ours", false),
            change(&repo, b"new/b", b"theirs", false),
        ],
    )
    .unwrap();
    assert_eq!(merged.tree, Some(expected));
    assert_eq!(
        merge_trees(&repo, base, base, ours).unwrap().tree,
        Some(ours)
    );
    assert_eq!(
        merge_trees(&repo, base, ours, ours).unwrap().tree,
        Some(ours)
    );
    assert_eq!(
        merge_trees(&repo, base, ours, base).unwrap().tree,
        Some(ours)
    );
    let deleted = apply(
        &repo,
        base,
        &[FileChange::Delete {
            path: RepoPath::new(b"dir/file".as_slice()).unwrap(),
        }],
    )
    .unwrap();
    let conflict = merge_trees(&repo, base, deleted, ours).unwrap();
    assert_eq!(conflict.tree, None);
    assert_eq!(conflict.conflicts[0].kind, ConflictKind::DeleteModify);
    assert_eq!(conflict.conflicts[0].path.as_bytes(), b"dir/file");
    // Git directories have no independent delete operation: removing the last
    // old child must still permit the other side's independently added child.
    let added = apply(&repo, base, &[change(&repo, b"dir/new", b"new", false)]).unwrap();
    let independent = merge_trees(&repo, base, deleted, added).unwrap();
    assert!(independent.conflicts.is_empty());
    assert_eq!(
        independent.tree,
        Some(apply(&repo, empty, &[change(&repo, b"dir/new", b"new", false)]).unwrap())
    );
    let a = apply(&repo, empty, &[change(&repo, b"node", b"file", false)]).unwrap();
    let b = apply(
        &repo,
        empty,
        &[change(&repo, b"node/child", b"child", false)],
    )
    .unwrap();
    assert_eq!(
        merge_trees(&repo, empty, a, b).unwrap().conflicts[0].kind,
        ConflictKind::FileDirectory
    );
    let a = apply(&repo, empty, &[change(&repo, b"same", b"ours", false)]).unwrap();
    let b = apply(&repo, empty, &[change(&repo, b"same", b"theirs", false)]).unwrap();
    assert_eq!(
        merge_trees(&repo, empty, a, b).unwrap().conflicts[0].kind,
        ConflictKind::AddAdd
    );
    let root = apply(&repo, empty, &[change(&repo, b"file", b"base", false)]).unwrap();
    let changed = apply(&repo, root, &[change(&repo, b"file", b"changed", false)]).unwrap();
    assert_eq!(
        merge_trees(&repo, root, empty, changed).unwrap().conflicts[0].kind,
        ConflictKind::DeleteModify
    );
    let conflict = merge_trees(
        &repo,
        root,
        changed,
        apply(&repo, root, &[change(&repo, b"file", b"different", false)]).unwrap(),
    )
    .unwrap();
    assert_eq!(conflict.conflicts[0].kind, ConflictKind::Content);
    assert_eq!(conflict.conflicts[0].lines[0].base, 0..1);
}

#[test]
fn text_clean_results_match_git_merge_file() {
    let cases: &[(&[u8], &[u8], &[u8])] = &[
        (b"a\nkeep\nc\n", b"A\nkeep\nc\n", b"a\nkeep\nC\n"),
        (b"a\nb\n", b"A\nb\n", b"a\nB\n"),
        (b"a\n", b"a\ninsert\n", b"a\nother\n"),
        (b"a\nkeep\nz", b"keep\nz", b"a\nkeep\nZ"),
        (b"", b"a", b"b"),
        (b"a\nx\nb\n", b"A\nx\nb\n", b"A\nx\nB\n"),
    ];
    let dir = tempfile::tempdir().unwrap();
    for (base, ours, theirs) in cases {
        for (name, bytes) in [("base", base), ("ours", ours), ("theirs", theirs)] {
            std::fs::write(dir.path().join(name), bytes).unwrap();
        }
        let reference = std::process::Command::new("git")
            .args(["merge-file", "-p", "ours", "base", "theirs"])
            .current_dir(dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            reference
                .status
                .code()
                .is_some_and(|code| (0..=127).contains(&code)),
            "{}",
            String::from_utf8_lossy(&reference.stderr)
        );
        match merge_text(base, ours, theirs) {
            TextMerge::Clean(bytes) => {
                assert!(reference.status.success());
                assert_eq!(bytes, reference.stdout);
            }
            TextMerge::Conflicted(_) => assert!(
                !reference.status.success(),
                "Git merged {:?}",
                reference.stdout
            ),
            TextMerge::BinaryConflict => panic!("fixture is text"),
        }
    }
}

#[test]
fn opaque_entries_and_binary_files_require_explicit_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let repo = Repository {
        meta: &meta,
        blobs: &blobs,
    };
    let tree = |entry| {
        repo.put_tree(&Tree {
            entries: [(b"entry".as_slice().into(), TreeEntry::Leaf(entry))].into(),
        })
        .unwrap()
    };
    let blob = |bytes: &[u8]| repo.put_blob(bytes).unwrap();
    for (base, ours, theirs, kind) in [
        (
            Entry::Symlink {
                blob: blob(b"base"),
            },
            Entry::Symlink {
                blob: blob(b"ours"),
            },
            Entry::Symlink {
                blob: blob(b"theirs"),
            },
            ConflictKind::Symlink,
        ),
        (
            Entry::Gitlink {
                commit: GitOid([0; 20]),
            },
            Entry::Gitlink {
                commit: GitOid([1; 20]),
            },
            Entry::Gitlink {
                commit: GitOid([2; 20]),
            },
            ConflictKind::Gitlink,
        ),
        (
            Entry::File {
                blob: blob(b"base\0"),
                executable: false,
            },
            Entry::File {
                blob: blob(b"ours\0"),
                executable: false,
            },
            Entry::File {
                blob: blob(b"theirs\0"),
                executable: false,
            },
            ConflictKind::Binary,
        ),
        (
            Entry::File {
                blob: blob(b"base"),
                executable: false,
            },
            Entry::Symlink {
                blob: blob(b"ours"),
            },
            Entry::File {
                blob: blob(b"theirs"),
                executable: false,
            },
            ConflictKind::TypeChange,
        ),
    ] {
        let base_id = tree(base.clone());
        let ours_id = tree(ours.clone());
        let theirs_id = tree(theirs.clone());
        let result = merge_trees(&repo, base_id, ours_id, theirs_id).unwrap();
        assert_eq!(result.tree, None);
        assert_eq!(
            result.conflicts,
            [TreeConflict {
                path: RepoPath::new(b"entry".as_slice()).unwrap(),
                kind,
                base: Some(TreeEntry::Leaf(base)),
                ours: Some(TreeEntry::Leaf(ours)),
                theirs: Some(TreeEntry::Leaf(theirs)),
                lines: vec![]
            }]
        );
        assert_eq!(
            merge_trees(&repo, base_id, base_id, ours_id).unwrap().tree,
            Some(ours_id)
        );
    }
}

proptest! {
    #[test]
    fn text_merge_identity_and_side_symmetry(base in prop::collection::vec(any::<u8>(), 0..80), ours in prop::collection::vec(any::<u8>(), 0..80), theirs in prop::collection::vec(any::<u8>(), 0..80)) {
        prop_assert_eq!(merge_text(&base, &base, &ours), TextMerge::Clean(ours.clone()));
        prop_assert_eq!(merge_text(&base, &ours, &base), TextMerge::Clean(ours.clone()));
        prop_assert_eq!(merge_text(&base, &ours, &ours), TextMerge::Clean(ours.clone()));
        let reverse = match merge_text(&base, &theirs, &ours) {
            TextMerge::Conflicted(conflicts) => TextMerge::Conflicted(conflicts.into_iter().map(|c| LineConflict { base: c.base, ours: c.theirs, theirs: c.ours }).collect()),
            other => other,
        };
        prop_assert_eq!(merge_text(&base, &ours, &theirs), reverse);
    }
}

#[test]
fn line_edits_match_git_across_insert_delete_replace_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let base = b"0\n1\n2\n3\n";
    std::fs::write(dir.path().join("base"), base).unwrap();
    let edit = |position: usize, kind: usize| {
        let mut lines: Vec<_> = base
            .split_inclusive(|b| *b == b'\n')
            .map(Vec::from)
            .collect();
        match kind {
            0 => {
                lines[position] = b"changed\n".to_vec();
            }
            1 => {
                lines.remove(position);
            }
            _ => {
                lines.insert(position, b"insert\n".to_vec());
            }
        }
        lines.concat()
    };
    for a in 0..4 {
        for b in 0..4 {
            for ka in 0..3 {
                for kb in 0..3 {
                    let ours = edit(a, ka);
                    let theirs = edit(b, kb);
                    std::fs::write(dir.path().join("ours"), &ours).unwrap();
                    std::fs::write(dir.path().join("theirs"), &theirs).unwrap();
                    let reference = std::process::Command::new("git")
                        .args(["merge-file", "-p", "ours", "base", "theirs"])
                        .current_dir(dir.path())
                        .env("GIT_CONFIG_NOSYSTEM", "1")
                        .env("GIT_CONFIG_GLOBAL", "/dev/null")
                        .output()
                        .unwrap();
                    match merge_text(base, &ours, &theirs) {
                        TextMerge::Clean(bytes) => {
                            assert!(reference.status.success(), "a={a} b={b} ka={ka} kb={kb}");
                            assert_eq!(bytes, reference.stdout);
                        }
                        TextMerge::Conflicted(_) => assert_eq!(
                            reference.status.code(),
                            Some(1),
                            "a={a} b={b} ka={ka} kb={kb}"
                        ),
                        TextMerge::BinaryConflict => panic!("fixture is text"),
                    }
                }
            }
        }
    }
}
