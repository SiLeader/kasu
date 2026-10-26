use kasu_core::*;
use kasu_derived::*;
use kasu_git::*;
use kasu_ingest::*;
use kasu_store::*;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn git(path: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn init(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(
        path,
        &[
            "init",
            "--bare",
            "--object-format=sha1",
            "--initial-branch=main",
        ],
        b"",
    );
}
fn object(path: &Path, kind: &str, raw: &[u8]) -> GitOid {
    let oid = git(
        path,
        &["hash-object", "--literally", "-w", "-t", kind, "--stdin"],
        raw,
    );
    oid_from_hex(oid.trim_ascii()).unwrap()
}
fn tree(path: &Path, entries: &[(&[u8], &[u8], GitOid)]) -> GitOid {
    let mut raw = Vec::new();
    for (mode, name, oid) in entries {
        raw.extend_from_slice(mode);
        raw.push(b' ');
        raw.extend_from_slice(name);
        raw.push(0);
        raw.extend(oid.0);
    }
    object(path, "tree", &raw)
}
fn commit(path: &Path, tree: GitOid, parents: &[GitOid], message: &[u8]) -> GitOid {
    let mut raw = format!("tree {tree}\n").into_bytes();
    for parent in parents {
        raw.extend(format!("parent {parent}\n").bytes());
    }
    raw.extend_from_slice(b"author A\xff <a@example.com> 1234567890 -0000\ncommitter B <b@example.com> 1234567891 +0900\nencoding ISO-8859-1\ngpgsig -----BEGIN PGP SIGNATURE-----\n fake-signature\n -----END PGP SIGNATURE-----\n\n");
    raw.extend(message);
    object(path, "commit", &raw)
}
fn setref(path: &Path, name: &str, oid: GitOid) {
    git(path, &["update-ref", name, &oid.to_string()], b"");
}
fn tag(path: &Path, target: GitOid, kind: &str, name: &str) -> GitOid {
    object(path,"tag",format!("object {target}\ntype {kind}\ntag {name}\ntagger T <t@example.com> 1234567892 +0000\n\n{name}\n").as_bytes())
}
fn fixture(path: &Path) -> Vec<GitOid> {
    init(path);
    let blob = object(path, "blob", b"hello\n");
    let new = object(path, "blob", b"new\n");
    let link = object(path, "blob", b"../file");
    let subtree = tree(path, &[(b"100644", b"nested", blob)]);
    let root = tree(
        path,
        &[
            (b"100644", b"a.c", blob),
            (b"40000", b"a", subtree),
            (b"120000", b"link", link),
            (b"160000", b"submodule", GitOid([3; 20])),
            (b"100755", b"z.sh", blob),
            (b"100644", b"\xff", blob),
        ],
    );
    let first = commit(path, root, &[], b"root\xff\n");
    let left_tree = tree(
        path,
        &[
            (b"100644", b"a.c", new),
            (b"40000", b"a", subtree),
            (b"120000", b"link", link),
            (b"160000", b"submodule", GitOid([3; 20])),
            (b"100755", b"z.sh", blob),
            (b"100644", b"\xff", blob),
        ],
    );
    let right_tree = tree(
        path,
        &[
            (b"100644", b"a.c", blob),
            (b"40000", b"a", subtree),
            (b"120000", b"link", link),
            (b"100644", b"second-parent", new),
            (b"160000", b"submodule", GitOid([3; 20])),
            (b"100755", b"z.sh", blob),
            (b"100644", b"\xff", blob),
        ],
    );
    let merged_tree = tree(
        path,
        &[
            (b"100644", b"a.c", new),
            (b"40000", b"a", subtree),
            (b"120000", b"link", link),
            (b"100644", b"second-parent", new),
            (b"160000", b"submodule", GitOid([3; 20])),
            (b"100755", b"z.sh", blob),
            (b"100644", b"\xff", blob),
        ],
    );
    let left = commit(path, left_tree, &[first], b"left\n");
    let right = commit(path, right_tree, &[first], b"right\n");
    let merge = commit(path, merged_tree, &[left, right], b"merge\n");
    let inner = tag(path, merge, "commit", "inner");
    let outer = tag(path, inner, "tag", "outer");
    setref(path, "refs/heads/main", merge);
    setref(path, "refs/heads/other", right);
    setref(path, "refs/tags/release", outer);
    setref(path, "refs/tags/tree", root);
    setref(path, "refs/tags/blob", blob);
    // Ensure gix can consume packed objects and packed refs, not just loose data.
    git(path, &["gc", "--prune=now"], b"");
    vec![first, left, right, merge]
}
fn object_ids(path: &Path) -> Vec<GitOid> {
    git(
        path,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
        b"",
    )
    .split(|b| *b == b'\n')
    .filter(|line| !line.is_empty())
    .map(|line| oid_from_hex(line).unwrap())
    .collect()
}
#[test]
fn packed_repository_roundtrips_every_object_and_ref_through_strict_pack() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.git");
    let commits = fixture(&source);
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path().join("native")).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let projection = ProjectionId::full(b"repo/\xff".as_slice());
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: projection.clone(),
        },
    };
    let report = importer.import(&source).unwrap();
    importer.publish(&meta, &report, b"ingest", 0).unwrap();
    assert_eq!(report.commits, 4);
    let ids = object_ids(&source);
    assert_eq!(report.objects, ids.len());
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
    };
    for oid in &ids {
        let (kind, raw) = importer.index.object(*oid).unwrap();
        assert_eq!(
            raw,
            git(&source, &["cat-file", kind.as_str(), &oid.to_string()], b"")
        );
    }
    for oid in commits {
        let id = importer.index.find_changeset(oid).unwrap().unwrap();
        assert_eq!(
            engine.derive::<GitCommit>(&ctx, id, &projection).unwrap().0,
            oid
        );
        let cs = ctx.repo.changeset(id).unwrap();
        assert_eq!(cs.author.tz.as_slice(), b"-0000");
        assert_eq!(cs.author.name.as_slice(), b"A\xff");
    }
    let exported = dir.path().join("exported.git");
    init(&exported);
    let mut pack = Vec::new();
    write_pack(&importer.index, ids, &mut pack).unwrap();
    git(&exported, &["index-pack", "--stdin", "--strict"], &pack);
    for (name, value) in &report.refs {
        match value {
            GitRef::Direct { target, .. } => {
                setref(&exported, std::str::from_utf8(name).unwrap(), *target)
            }
            GitRef::Symbolic { target } => {
                git(
                    &exported,
                    &[
                        "symbolic-ref",
                        std::str::from_utf8(name).unwrap(),
                        std::str::from_utf8(target).unwrap(),
                    ],
                    b"",
                );
            }
        }
    }
    git(&exported, &["fsck", "--strict"], b"");
    assert_eq!(
        git(&source, &["show-ref", "--dereference"], b""),
        git(&exported, &["show-ref", "--dereference"], b"")
    );
    let refs = list_refs(&engine, &ctx, &meta, &projection).unwrap();
    assert_eq!(
        refs.iter().map(|r| (&r.name, &r.value)).collect::<Vec<_>>(),
        report.refs.iter().map(|(n, v)| (n, v)).collect::<Vec<_>>()
    );
    let other = GitIndex {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
        projection: ProjectionId::full(b"other".as_slice()),
    };
    assert!(other.find(object_ids(&source)[0]).unwrap().is_none());
    let before = (
        meta.tail(&projection.repo, 0, 100).unwrap().len(),
        meta.tail_git(&projection.repo, 0, 100).unwrap().len(),
    );
    let again = importer.import(&source).unwrap();
    importer.publish(&meta, &again, b"ingest", 0).unwrap();
    assert_eq!(
        before,
        (
            meta.tail(&projection.repo, 0, 100).unwrap().len(),
            meta.tail_git(&projection.repo, 0, 100).unwrap().len()
        )
    );
    let clone_path = dir.path().join("clone");
    git(
        dir.path(),
        &[
            "clone",
            "--bare",
            exported.to_str().unwrap(),
            clone_path.to_str().unwrap(),
        ],
        b"",
    );
    git(&clone_path, &["fsck", "--strict"], b"");
}
#[test]
fn noncanonical_trees_keep_raw_variants_without_poisoning_native_projection() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.git");
    init(&source);
    let blob = object(&source, "blob", b"x");
    // Both trees map to the same native Tree, but have different raw ordering/modes.
    let canonical = tree(&source, &[(b"100644", b"a", blob), (b"100644", b"b", blob)]);
    let unusual = tree(&source, &[(b"100664", b"b", blob), (b"100644", b"a", blob)]);
    let a = commit(&source, canonical, &[], b"a");
    let b = commit(&source, unusual, &[a], b"b");
    setref(&source, "refs/heads/main", b);
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path().join("native")).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: ProjectionId::full(b"repo".as_slice()),
        },
    };
    importer.import(&source).unwrap();
    let ta = importer.index.require(canonical).unwrap().native;
    let tb = importer.index.require(unusual).unwrap().native;
    assert_eq!(ta, tb);
    assert_ne!(canonical, unusual);
    let NativeTarget::Tree(root) = ta else {
        panic!()
    };
    assert_eq!(synthesize_tree(&importer.index, root).unwrap(), canonical);
    assert_eq!(
        GitOid::hash("tree", &importer.index.object(unusual).unwrap().1),
        unusual
    );
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
    };
    assert_eq!(
        engine
            .derive::<GitTree>(
                &ctx,
                importer.index.find_changeset(b).unwrap().unwrap(),
                &importer.index.projection
            )
            .unwrap()
            .0,
        unusual
    );
}
#[test]
fn rejects_missing_wrong_type_and_unrepresentable_trees_before_publication() {
    for case in ["missing", "wrong-type", "empty-directory", "duplicate-name"] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.git");
        init(&source);
        let blob = object(&source, "blob", b"x");
        let root = match case {
            "missing" => tree(&source, &[(b"100644", b"file", GitOid([0; 20]))]),
            "wrong-type" => tree(&source, &[(b"40000", b"dir", blob)]),
            "empty-directory" => {
                let empty = tree(&source, &[]);
                tree(&source, &[(b"40000", b"dir", empty)])
            }
            _ => tree(
                &source,
                &[(b"100644", b"same", blob), (b"100644", b"same", blob)],
            ),
        };
        let commit = commit(&source, root, &[], b"invalid");
        setref(&source, "refs/heads/main", commit);
        let meta = SqliteStore::memory().unwrap();
        let blobs = FsBlobStore::open(dir.path().join("native")).unwrap();
        let mut engine = Engine::standard();
        register_git(&mut engine);
        let importer = Importer {
            engine: &engine,
            index: GitIndex {
                repo: Repository {
                    meta: &meta,
                    blobs: &blobs,
                },
                projection: ProjectionId::full(b"repo".as_slice()),
            },
        };
        assert!(importer.import(&source).is_err(), "{case}");
        assert!(meta.list(b"repo", b"").unwrap().is_empty());
        assert!(meta.list_git(b"repo", b"").unwrap().is_empty());
    }
}
#[test]
fn empty_repository_has_unborn_head_and_cli_imports_it() {
    let dir = tempfile::tempdir().unwrap();
    let source: PathBuf = dir.path().join("empty.git");
    init(&source);
    let destination = dir.path().join("native");
    let output = Command::new(env!("CARGO_BIN_EXE_kasu-ingest"))
        .arg(&source)
        .arg(&destination)
        .arg("empty")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("0 objects, 0 commits, 1 refs")
    );
    let meta = SqliteStore::open(destination.join("meta.db")).unwrap();
    assert_eq!(
        meta.resolve_git(b"empty", b"HEAD").unwrap().value,
        Some(GitRef::Symbolic {
            target: b"refs/heads/main".to_vec()
        })
    );
}

#[test]
fn cli_registers_repositories_and_imports_them_independently() {
    let dir = tempfile::tempdir().unwrap();
    let source: PathBuf = dir.path().join("empty.git");
    init(&source);
    let destination = dir.path().join("native");
    let ingest = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kasu-ingest"))
            .args(args)
            .arg(&source)
            .arg(&destination)
            .output()
            .unwrap()
    };
    let with = |name: &str, create: bool| {
        let mut args = vec![];
        if create {
            args.push("--create");
        }
        let output = Command::new(env!("CARGO_BIN_EXE_kasu-ingest"))
            .args(&args)
            .arg(&source)
            .arg(&destination)
            .arg(name)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    assert!(!ingest(&[]).status.success());
    // The first import into an empty registry keeps the legacy key layout.
    assert!(with("legacy", false).0);
    // Afterwards, new names must be created explicitly and get fresh keys.
    let (ok, stderr) = with("team/app", false);
    assert!(!ok);
    assert!(stderr.contains("--create"), "{stderr}");
    assert!(with("team/app", true).0);
    assert!(with("team/app", false).0);
    let meta = SqliteStore::open(destination.join("meta.db")).unwrap();
    let key = |name: &str| {
        meta.resolve_repository_name(&kasu_core::RepoName::new(name).unwrap(), 0)
            .unwrap()
            .unwrap()
            .key
    };
    assert_eq!(key("legacy").as_bytes(), b"legacy");
    let app = key("team/app");
    assert!(!app.is_legacy());
    assert!(
        meta.resolve_git(app.as_bytes(), b"HEAD")
            .unwrap()
            .value
            .is_some()
    );
    assert!(
        meta.resolve_git(b"team/app", b"HEAD")
            .unwrap()
            .value
            .is_none()
    );
    assert!(!with("bad name", true).0);
}

#[test]
#[ignore = "requires pinned libgit2/TestGitRepository checkout in KASU_OSS_REPO"]
fn pinned_oss_repository_roundtrips_all_objects_through_pack_and_clone() {
    let source = PathBuf::from(std::env::var_os("KASU_OSS_REPO").expect(
        "set KASU_OSS_REPO to a bare clone of https://github.com/libgit2/TestGitRepository.git",
    ));
    verify_pinned_roundtrip(
        &source,
        include_bytes!("fixtures/oss-refs.txt"),
        b"TestGitRepository",
        21,
        70,
        None,
        |_| {},
    );
}

#[test]
#[ignore = "requires tools/oss-fixtures/prepare.py output in KASU_OSS_SUBMODULE_REPO"]
fn pinned_upstream_submodule_fixture_roundtrips_all_reachable_objects() {
    let source = PathBuf::from(
        std::env::var_os("KASU_OSS_SUBMODULE_REPO")
            .expect("prepare the pinned upstream submodule fixture"),
    );
    verify_pinned_roundtrip(
        &source,
        include_bytes!("fixtures/upstream-submodule-refs.txt"),
        b"libgit2-submodules",
        2,
        6,
        Some((
            b"testrepo",
            oid_from_hex(b"a65fedf39aefe402d3bb6e24df4d4f5fe4547750").unwrap(),
        )),
        |_| {},
    );
}
#[test]
#[ignore = "requires tools/oss-fixtures/prepare_byte_paths.py output in KASU_OSS_BYTE_PATH_REPO"]
fn pinned_gitlab_byte_paths_tags_and_merges_roundtrip() {
    let source = PathBuf::from(
        std::env::var_os("KASU_OSS_BYTE_PATH_REPO")
            .expect("prepare the fixed GitLab byte-path graph"),
    );
    assert_eq!(
        git(
            &source,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname) %(objecttype)"
            ],
            b""
        ),
        include_bytes!("fixtures/upstream-byte-path-objects.txt")
    );
    assert_eq!(
        git(&source, &["symbolic-ref", "HEAD"], b""),
        b"refs/heads/invalid-utf8-diff-paths\n"
    );
    verify_pinned_roundtrip(
        &source,
        include_bytes!("fixtures/upstream-byte-path-refs.txt"),
        b"gitlab-byte-paths",
        50,
        183,
        None,
        |importer| {
            let ctx = Ctx {
                repo: Repository {
                    meta: importer.index.repo.meta,
                    blobs: importer.index.repo.blobs,
                },
            };
            let path = RepoPath::new(b"hello\x80world".as_slice()).unwrap();
            assert!(std::str::from_utf8(path.as_bytes()).is_err());
            for (commit, expected) in [
                (
                    b"d0888d297eadcd7a345427915c309413b1231e65",
                    Some(b"3742e48c1108ced3bf45ac633b34b65ac3f2af04"),
                ),
                (
                    b"19950f03c765f7ac8723a73a0599764095f52fc0",
                    Some(b"ea20973903d3800ac659d503596f9105f0a7423f"),
                ),
                (b"99e4853698007ed7e68826730ef5313d24841991", None),
            ] {
                let id = importer
                    .index
                    .find_changeset(oid_from_hex(commit).unwrap())
                    .unwrap()
                    .unwrap();
                let root = importer.engine.derive::<RootTree>(&ctx, id, &()).unwrap().0;
                let entry = kasu_derived::lookup(&ctx.repo, root, &path).unwrap();
                if let Some(expected) = expected {
                    let Some(TreeEntry::Leaf(Entry::File {
                        blob,
                        executable: false,
                    })) = entry
                    else {
                        panic!("original byte path must remain a regular file");
                    };
                    assert_eq!(
                        GitOid::hash("blob", &ctx.repo.blob(blob).unwrap()),
                        oid_from_hex(expected).unwrap()
                    );
                    assert!(
                        ctx.repo
                            .changeset(id)
                            .unwrap()
                            .changes
                            .iter()
                            .any(|c| matches!(c, FileChange::Set { path: p, .. } if p == &path))
                    );
                } else {
                    assert_eq!(entry, None);
                    assert!(
                        ctx.repo
                            .changeset(id)
                            .unwrap()
                            .changes
                            .iter()
                            .any(|c| matches!(c, FileChange::Delete { path: p } if p == &path))
                    );
                }
            }
            let head = importer
                .index
                .find_changeset(oid_from_hex(b"99e4853698007ed7e68826730ef5313d24841991").unwrap())
                .unwrap()
                .unwrap();
            let root = importer
                .engine
                .derive::<RootTree>(&ctx, head, &())
                .unwrap()
                .0;
            for (path, target) in [
                (
                    b"gitlab-grack".as_slice(),
                    b"645f6c4c82fd3f5e06f67134450a570b795e55a6",
                ),
                (
                    b"gitlab-shell".as_slice(),
                    b"79bceae69cb5750d6567b223597999bfa91cb3b9",
                ),
                (
                    b"six".as_slice(),
                    b"409f37c4f05865e4fb208c771485f211a22c4c2d",
                ),
            ] {
                let target = oid_from_hex(target).unwrap();
                assert_eq!(
                    kasu_derived::lookup(&ctx.repo, root, &RepoPath::new(path).unwrap()).unwrap(),
                    Some(TreeEntry::Leaf(Entry::Gitlink { commit: target }))
                );
                assert!(importer.index.find_changeset(target).unwrap().is_none());
            }
            let mut merges = 0;
            let mut commits = 0;
            for row in git(&source, &["rev-list", "--parents", "--all"], b"")
                .split(|b| *b == b'\n')
                .filter(|row| !row.is_empty())
            {
                let ids: Vec<_> = row
                    .split(|b| *b == b' ')
                    .map(|oid| {
                        importer
                            .index
                            .find_changeset(oid_from_hex(oid).unwrap())
                            .unwrap()
                            .unwrap()
                    })
                    .collect();
                assert_eq!(ctx.repo.changeset(ids[0]).unwrap().parents, ids[1..]);
                commits += 1;
                merges += usize::from(ids.len() > 2);
            }
            assert_eq!((commits, merges), (50, 14));
        },
    );
}
fn verify_pinned_roundtrip(
    source: &Path,
    expected_refs: &[u8],
    repository: &[u8],
    commits: usize,
    objects: usize,
    gitlink: Option<(&[u8], GitOid)>,
    verify_native: impl Fn(&Importer<'_>),
) {
    assert_eq!(git(source, &["show-ref"], b""), expected_refs);
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path().join("native")).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: ProjectionId::full(repository),
        },
    };
    let report = importer.import(source).unwrap();
    importer.publish(&meta, &report, b"oss-ingest", 0).unwrap();
    assert_eq!(report.commits, commits);
    assert_eq!(report.objects, objects);
    git(source, &["fsck", "--strict"], b"");
    if let Some((path, target)) = gitlink {
        let head = oid_from_hex(git(source, &["rev-parse", "HEAD"], b"").trim_ascii()).unwrap();
        let native = importer.index.find_changeset(head).unwrap().unwrap();
        let root = engine
            .derive::<RootTree>(
                &Ctx {
                    repo: Repository {
                        meta: importer.index.repo.meta,
                        blobs: importer.index.repo.blobs,
                    },
                },
                native,
                &(),
            )
            .unwrap()
            .0;
        assert_eq!(
            kasu_derived::lookup(&importer.index.repo, root, &RepoPath::new(path).unwrap())
                .unwrap(),
            Some(TreeEntry::Leaf(Entry::Gitlink { commit: target }))
        );
        assert!(importer.index.find_changeset(target).unwrap().is_none());
    }
    verify_native(&importer);
    let ids = object_ids(source);
    assert_eq!(ids.len(), report.objects);
    for oid in &ids {
        let (kind, raw) = importer.index.object(*oid).unwrap();
        assert_eq!(
            raw,
            git(source, &["cat-file", kind.as_str(), &oid.to_string()], b"")
        );
    }
    let exported = dir.path().join("exported.git");
    init(&exported);
    let mut pack = Vec::new();
    write_pack(&importer.index, ids, &mut pack).unwrap();
    git(&exported, &["index-pack", "--stdin", "--strict"], &pack);
    for (name, value) in &report.refs {
        match value {
            GitRef::Direct { target, .. } => {
                setref(&exported, std::str::from_utf8(name).unwrap(), *target)
            }
            GitRef::Symbolic { target } => {
                git(
                    &exported,
                    &[
                        "symbolic-ref",
                        std::str::from_utf8(name).unwrap(),
                        std::str::from_utf8(target).unwrap(),
                    ],
                    b"",
                );
            }
        }
    }
    git(&exported, &["fsck", "--strict"], b"");
    assert_eq!(
        git(source, &["symbolic-ref", "HEAD"], b""),
        git(&exported, &["symbolic-ref", "HEAD"], b"")
    );
    assert_eq!(
        git(source, &["show-ref", "--dereference"], b""),
        git(&exported, &["show-ref", "--dereference"], b"")
    );
    let clone = dir.path().join("clone.git");
    git(
        dir.path(),
        &[
            "clone",
            "--bare",
            exported.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
        b"",
    );
    git(&clone, &["fsck", "--strict"], b"");
    assert_eq!(
        git(source, &["symbolic-ref", "HEAD"], b""),
        git(&clone, &["symbolic-ref", "HEAD"], b"")
    );
    let mut source_ids = object_ids(source);
    let mut clone_ids = object_ids(&clone);
    source_ids.sort();
    clone_ids.sort();
    assert_eq!(source_ids, clone_ids);
}

#[test]
fn native_projected_commits_keep_identity_when_imported_back() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.git");
    init(&source);
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path().join("native")).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
    };
    let blob = ctx.repo.put_blob(b"native\n").unwrap();
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
        changes: vec![FileChange::Set {
            path: RepoPath::new(b"f".as_slice()).unwrap(),
            entry: Entry::File {
                blob,
                executable: false,
            },
        }],
        git_provenance: None,
    };
    let id = ctx.repo.put_changeset(&cs).unwrap();
    let projection = ProjectionId::full(b"repo".as_slice());
    let commit = engine.derive::<GitCommit>(&ctx, id, &projection).unwrap().0;
    let root = engine.derive::<GitTree>(&ctx, id, &projection).unwrap().0;
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: projection.clone(),
        },
    };
    let mut pack = Vec::new();
    write_pack(
        &importer.index,
        [commit, root, GitOid::hash("blob", b"native\n")],
        &mut pack,
    )
    .unwrap();
    git(&source, &["index-pack", "--stdin", "--strict"], &pack);
    setref(&source, "refs/heads/main", commit);
    let report = importer.import(&source).unwrap();
    importer.publish(&meta, &report, b"a", 0).unwrap();
    assert_eq!(report.commits, 1);
    assert_eq!(
        meta.resolve(b"repo", b"refs/heads/main").unwrap().target,
        Some(id)
    );
    assert!(ctx.repo.changeset(id).unwrap().git_provenance.is_none());
    // A descendant from Git must attach to the original native identity, too.
    let child = commit_raw_child(&source, root, commit);
    setref(&source, "refs/heads/main", child);
    let report = importer.import(&source).unwrap();
    importer.publish(&meta, &report, b"a", 1).unwrap();
    let child_id = importer.index.find_changeset(child).unwrap().unwrap();
    assert_eq!(ctx.repo.changeset(child_id).unwrap().parents, vec![id]);
}
fn commit_raw_child(source: &Path, tree: GitOid, parent: GitOid) -> GitOid {
    commit(source, tree, &[parent], b"Git child\n")
}

#[test]
fn publication_revalidates_ref_kind_peeling_and_symbolic_targets() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.git");
    fixture(&source);
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path().join("native")).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: ProjectionId::full(b"repo".as_slice()),
        },
    };
    let mut report = importer.import(&source).unwrap();
    let original = report.refs.clone();
    let mut missing = original.clone();
    missing.push((
        b"refs/tags/missing".to_vec(),
        GitRef::Direct {
            target: GitOid([7; 20]),
            peeled: None,
            kind: GitKind::Blob,
        },
    ));
    let mut bad_peel = original.clone();
    for (_, value) in &mut bad_peel {
        if let GitRef::Direct {
            kind: GitKind::Tag,
            peeled,
            ..
        } = value
        {
            *peeled = None;
        }
    }
    let mut cycle = original.clone();
    cycle[0] = (
        b"HEAD".to_vec(),
        GitRef::Symbolic {
            target: b"HEAD".to_vec(),
        },
    );
    let mut wrong_kind = original.clone();
    for (_, value) in &mut wrong_kind {
        if let GitRef::Direct { kind, .. } = value
            && *kind == GitKind::Commit
        {
            *kind = GitKind::Blob;
        }
    }
    for refs in [missing, bad_peel, cycle, wrong_kind] {
        report.refs = refs;
        assert!(importer.publish(&meta, &report, b"a", 0).is_err());
        assert!(meta.list(b"repo", b"").unwrap().is_empty());
        assert!(meta.list_git(b"repo", b"").unwrap().is_empty());
    }
    report.refs = original;
    importer.publish(&meta, &report, b"a", 0).unwrap();
}

#[test]
fn native_edits_after_import_reuse_unchanged_merkle_subtrees_and_blobs() {
    use std::sync::Mutex;
    struct Reads<'a> {
        inner: &'a FsBlobStore,
        ids: Mutex<Vec<ObjectId>>,
    }
    impl BlobStore for Reads<'_> {
        fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
            self.ids.lock().unwrap().push(id);
            self.inner.get_compressed(id)
        }
        fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
            self.inner.put_compressed(id, bytes)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.git");
    fixture(&source);
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path().join("native")).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let projection = ProjectionId::full(b"repo".as_slice());
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: projection.clone(),
        },
    };
    let report = importer.import(&source).unwrap();
    importer.publish(&meta, &report, b"a", 0).unwrap();
    let parent = meta
        .resolve(b"repo", b"refs/heads/main")
        .unwrap()
        .target
        .unwrap();
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
    };
    let root = engine.derive::<RootTree>(&ctx, parent, &()).unwrap().0;
    let TreeEntry::SubTree(unchanged) =
        lookup(&ctx.repo, root, &RepoPath::new(b"a".as_slice()).unwrap())
            .unwrap()
            .unwrap()
    else {
        panic!()
    };
    let mut cs = ctx.repo.changeset(parent).unwrap();
    cs.parents = vec![parent];
    cs.git_provenance = None;
    let changed = ctx.repo.put_blob(b"new native content\n").unwrap();
    cs.changes = vec![FileChange::Set {
        path: RepoPath::new(b"a.c".as_slice()).unwrap(),
        entry: Entry::File {
            blob: changed,
            executable: false,
        },
    }];
    let id = ctx.repo.put_changeset(&cs).unwrap();
    engine.derive::<RootTree>(&ctx, id, &()).unwrap();
    let reads = Reads {
        inner: &blobs,
        ids: Mutex::new(Vec::new()),
    };
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &reads,
        },
    };
    engine.derive::<GitCommit>(&ctx, id, &projection).unwrap();
    let fetched = reads.ids.lock().unwrap();
    assert!(
        !fetched.contains(&ObjectId(unchanged.0)),
        "unchanged native subtree was walked again"
    );
    assert!(
        !fetched.contains(&ObjectId(BlobId::hash(b"hello\n").0)),
        "unchanged blob was read again"
    );
    assert!(
        fetched.contains(&ObjectId(changed.0)),
        "new blob must be indexed"
    );
}

#[test]
fn rejects_shallow_and_sha256_repositories_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.git");
    fixture(&source);
    let shallow = dir.path().join("shallow.git");
    git(
        dir.path(),
        &[
            "clone",
            "--bare",
            "--depth=1",
            &format!("file://{}", source.display()),
            shallow.to_str().unwrap(),
        ],
        b"",
    );
    let sha256 = dir.path().join("sha256.git");
    std::fs::create_dir_all(&sha256).unwrap();
    git(
        &sha256,
        &[
            "init",
            "--bare",
            "--object-format=sha256",
            "--initial-branch=main",
        ],
        b"",
    );
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path().join("native")).unwrap();
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: &meta,
                blobs: &blobs,
            },
            projection: ProjectionId::full(b"repo".as_slice()),
        },
    };
    for path in [shallow, sha256] {
        assert!(importer.import(path).is_err());
        assert!(meta.list(b"repo", b"").unwrap().is_empty());
        assert!(meta.list_git(b"repo", b"").unwrap().is_empty());
    }
}
