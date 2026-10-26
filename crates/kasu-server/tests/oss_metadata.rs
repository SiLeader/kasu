//! Differential acceptance tests over an externally authored, pinned object graph.
use kasu_derived::*;
use kasu_git::*;
use kasu_ingest::Importer;
use kasu_proto::v1 as p;
use kasu_server::Backend;
use kasu_store::*;
use std::{collections::BTreeSet, path::Path, process::Command, sync::Arc};

fn git(source: &Path, args: &[&str], no_common_base: bool) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success()
            || (no_common_base
                && output.status.code() == Some(1)
                && output.stdout.is_empty()
                && output.stderr.is_empty()),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
#[ignore = "requires the fixed TestGitRepository graph in KASU_OSS_REPO; see docs/git-import.md"]
fn pinned_oss_metadata_matches_git_for_all_commits_paths_and_tip_pairs() {
    let source = std::path::PathBuf::from(std::env::var_os("KASU_OSS_REPO").unwrap());
    assert_eq!(
        git(&source, &["show-ref"], false),
        include_bytes!("../../kasu-ingest/tests/fixtures/oss-refs.txt")
    );
    git(&source, &["fsck", "--strict"], false);
    let dir = tempfile::tempdir().unwrap();
    let meta = Arc::new(SqliteStore::memory().unwrap());
    let blobs = Arc::new(FsBlobStore::open(dir.path()).unwrap());
    let mut engine = Engine::standard();
    register_git(&mut engine);
    let importer = Importer {
        engine: &engine,
        index: GitIndex {
            repo: Repository {
                meta: meta.as_ref(),
                blobs: blobs.as_ref(),
            },
            projection: ProjectionId::full(b"oss-metadata"),
        },
    };
    let report = importer.import(&source).unwrap();
    assert_eq!((report.commits, report.objects), (21, 70));
    importer
        .publish(meta.as_ref(), &report, b"acceptance", 0)
        .unwrap();
    let oids: Vec<_> = git(&source, &["rev-list", "--all"], false)
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| oid_from_hex(l).unwrap())
        .collect();
    assert_eq!(oids.len(), 21);
    let ids: Vec<_> = oids
        .iter()
        .map(|oid| importer.index.find_changeset(*oid).unwrap().unwrap())
        .collect();
    let original_oid = |id: &[u8]| oids[ids.iter().position(|cs| cs.0.as_slice() == id).unwrap()];
    let backend = Backend::new(
        b"oss-metadata".to_vec(),
        meta.clone(),
        blobs.clone(),
        meta.clone(),
        None,
    );
    let mut paths = BTreeSet::new();
    let mut files = Vec::new();
    for oid in &oids {
        let tree = git(
            &source,
            &["ls-tree", "-rz", "--full-tree", &oid.to_string()],
            false,
        );
        let mut at_commit = Vec::new();
        for row in tree.split(|b| *b == 0).filter(|row| !row.is_empty()) {
            let (metadata, path) = row.split_at(row.iter().position(|b| *b == b'\t').unwrap());
            let path = path[1..].to_vec();
            paths.insert(path.clone());
            if metadata.starts_with(b"100644 blob ") || metadata.starts_with(b"100755 blob ") {
                at_commit.push(path);
            }
        }
        files.push(at_commit);
    }
    // Also compare the API's empty-history behavior, independently of the fixture paths.
    paths.insert(b"kasu-acceptance-missing-path".to_vec());
    let mut log_queries = 0;
    let mut blame_queries = 0;
    let mut base_queries = 0;
    for (n, oid) in oids.iter().enumerate() {
        for path in &paths {
            let mut cursor = Vec::new();
            let mut actual = Vec::new();
            let mut seen_cursors = BTreeSet::new();
            loop {
                let page = backend
                    .log(p::LogRequest {
                        repo: b"oss-metadata".to_vec(),
                        start: ids[n].0.to_vec(),
                        path: Some(path.clone()),
                        limit: 2,
                        cursor,
                    })
                    .unwrap();
                actual.extend(
                    page.changesets
                        .iter()
                        .map(|cs| original_oid(&cs.id).to_string()),
                );
                cursor = page.cursor;
                if cursor.is_empty() {
                    break;
                }
                assert!(seen_cursors.insert(cursor.clone()), "repeated log cursor");
                assert!(
                    actual.len() <= oids.len(),
                    "path log cursor did not advance"
                );
            }
            let expected = git(
                &source,
                &[
                    "log",
                    "--first-parent",
                    "--format=%H",
                    &oid.to_string(),
                    "--",
                    std::str::from_utf8(path).unwrap(),
                ],
                false,
            );
            assert_eq!(
                actual,
                String::from_utf8(expected)
                    .unwrap()
                    .lines()
                    .collect::<Vec<_>>(),
                "log {oid} {path:?}"
            );
            log_queries += 1;
        }
        for path in &files[n] {
            let actual = backend
                .blame(p::PathRequest {
                    repo: b"oss-metadata".to_vec(),
                    id: ids[n].0.to_vec(),
                    path: path.clone(),
                    follow_copies: false,
                    follow_moves: false,
                })
                .unwrap();
            let raw = git(
                &source,
                &[
                    "-c",
                    "core.quotePath=false",
                    "blame",
                    "--line-porcelain",
                    &oid.to_string(),
                    "--",
                    std::str::from_utf8(path).unwrap(),
                ],
                false,
            );
            let mut expected = Vec::new();
            let mut origin = None;
            let mut origin_path = None;
            for line in raw.split(|b| *b == b'\n') {
                let fields: Vec<_> = line.split(|b| *b == b' ').collect();
                if fields.len() >= 3
                    && fields[0].len() == 40
                    && let Ok(oid) = oid_from_hex(fields[0])
                {
                    origin = Some((
                        oid,
                        std::str::from_utf8(fields[1])
                            .unwrap()
                            .parse::<u64>()
                            .unwrap(),
                    ));
                    origin_path = None;
                } else if let Some(path) = line.strip_prefix(b"filename ") {
                    assert!(
                        !path.starts_with(b"\""),
                        "extend parser for quoted fixture paths"
                    );
                    origin_path = Some(path.to_vec());
                } else if line.starts_with(b"\t") {
                    let (oid, line) = origin.unwrap();
                    expected.push((oid, line, origin_path.clone().unwrap()));
                }
            }
            let actual: Vec<_> = actual
                .lines
                .iter()
                .map(|l| (original_oid(&l.changeset), l.line, l.path.clone()))
                .collect();
            assert_eq!(actual, expected, "blame {oid} {path:?}");
            blame_queries += 1;
        }
        for (m, other) in oids.iter().enumerate() {
            let actual = backend
                .merge_base(p::CompareRequest {
                    repo: b"oss-metadata".to_vec(),
                    base: ids[n].0.to_vec(),
                    head: ids[m].0.to_vec(),
                })
                .unwrap();
            let actual: BTreeSet<_> = actual.ids.iter().map(|id| original_oid(id)).collect();
            let expected = git(
                &source,
                &["merge-base", "--all", &oid.to_string(), &other.to_string()],
                true,
            );
            let expected: BTreeSet<_> = expected
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .map(|l| oid_from_hex(l).unwrap())
                .collect();
            assert_eq!(actual, expected, "merge-base {oid} {other}");
            base_queries += 1;
        }
    }
    println!(
        "fixed OSS metadata: commits={}, paths={} (including missing), log={log_queries}, blame={blame_queries}, merge-base={base_queries}",
        oids.len(),
        paths.len()
    );
    assert!(blame_queries > oids.len());
    assert_eq!(base_queries, 441);
}
