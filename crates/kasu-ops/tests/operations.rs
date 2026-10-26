use kasu_core::*;
use kasu_derived::*;
use kasu_ops::*;
use kasu_store::*;
use proptest::prelude::*;
use std::collections::BTreeSet;
fn cs(parents: Vec<ChangesetId>, n: u32) -> Changeset {
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
        message: n.to_be_bytes().as_slice().into(),
        changes: vec![],
        git_provenance: None,
    }
}
#[test]
fn all_merge_bases_in_criss_cross_history() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
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
    let root = ops.create_changeset(&cs(vec![], 0)).unwrap();
    let a = ops.create_changeset(&cs(vec![root], 1)).unwrap();
    let b = ops.create_changeset(&cs(vec![root], 2)).unwrap();
    let x = ops.create_changeset(&cs(vec![a, b], 3)).unwrap();
    let y = ops.create_changeset(&cs(vec![b, a], 4)).unwrap();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(ops.merge_bases(x, y).unwrap(), expected);
    assert!(ops.is_ancestor(b, x).unwrap());
    assert!(!ops.is_ancestor(a, b).unwrap());
    let mut bad = cs(vec![ChangesetId([0; 32])], 5);
    assert!(ops.create_changeset(&bad).is_err());
    bad.parents = vec![];
    bad.changes = vec![FileChange::Delete {
        path: RepoPath::new(b"missing".as_slice()).unwrap(),
    }];
    assert!(ops.create_changeset(&bad).is_err());
    assert!(ops.ctx.repo.changeset(bad.id()).is_err());
    bad.changes = vec![FileChange::Set {
        path: RepoPath::new(b"missing".as_slice()).unwrap(),
        entry: Entry::Symlink {
            blob: BlobId([0; 32]),
        },
    }];
    assert!(ops.create_changeset(&bad).is_err());
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn ancestry_matches_transitive_closure(edges in prop::collection::vec((any::<usize>(),any::<usize>()),1..45)) {
        let dir = tempfile::tempdir().unwrap(); let meta = SqliteStore::memory().unwrap(); let blobs = FsBlobStore::open(dir.path()).unwrap(); let engine = Engine::standard(); let ops = Operations { engine: &engine, ctx: Ctx { repo: Repository { meta: &meta, blobs: &blobs } } };
        let mut ids = Vec::new(); let mut closure: Vec<BTreeSet<usize>> = Vec::new();
        for (n,(a,b)) in edges.iter().enumerate() {
            let parent_indices: BTreeSet<_> = if n == 0 { BTreeSet::new() } else { BTreeSet::from([a%n,b%n]) };
            let mut ancestors = BTreeSet::from([n]);
            for p in &parent_indices { ancestors.extend(&closure[*p]); }
            ids.push(ops.create_changeset(&cs(parent_indices.iter().map(|p|ids[*p]).collect(),n as u32)).unwrap());
            closure.push(ancestors);
        }
        for (b,ancestors) in closure.iter().enumerate() {
            for (a,id) in ids.iter().enumerate() { prop_assert_eq!(ops.is_ancestor(*id,ids[b]).unwrap(), ancestors.contains(&a)); }
        }
        let head = ids.len()-1;
        for b in 0..ids.len() {
            let common: BTreeSet<_> = closure[head].intersection(&closure[b]).copied().collect();
            let mut expected: Vec<_> = common.iter().filter(|candidate| !common.iter().any(|other| candidate != &other && closure[*other].contains(candidate))).map(|i| ids[*i]).collect();
            expected.sort();
            prop_assert_eq!(ops.merge_bases(ids[head],ids[b]).unwrap(), expected);
        }
        for a in 0..ids.len() {
            for b in [None, Some(head), Some(a / 2)] {
                let excluded = b.map(|b| closure[b].clone()).unwrap_or_default();
                let expected: BTreeSet<_> = closure[a].difference(&excluded).map(|i| ids[*i]).collect();
                prop_assert_eq!(ops.ancestors_excluding(ids[a], b.map(|b| ids[b])).unwrap(), expected);
            }
        }
    }
}

struct ReadCounts<'a> {
    store: &'a dyn MetaStore,
    gets: std::sync::atomic::AtomicU64,
}
impl MetaStore for ReadCounts<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.store.get(key)
    }
    fn put_if_absent(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.store.put_if_absent(key, value)
    }
}
#[test]
fn merge_base_stops_at_a_recent_fork_in_a_long_persisted_history() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::memory().unwrap();
    let meta = ReadCounts {
        store: &store,
        gets: std::sync::atomic::AtomicU64::new(0),
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
    let mut tip = None;
    for n in 0..5000 {
        let id = ops
            .create_changeset(&cs(tip.into_iter().collect(), n))
            .unwrap();
        tip = Some(id);
        if [9, 99, 999, 4999].contains(&n) {
            let a = ops.create_changeset(&cs(vec![id], 10_000 + n)).unwrap();
            let b = ops.create_changeset(&cs(vec![id], 20_000 + n)).unwrap();
            let b = ops.create_changeset(&cs(vec![b], 30_000 + n)).unwrap();
            // A fresh engine still reads the already persisted Generation index.
            let cold = Engine::standard();
            let query = Operations {
                engine: &cold,
                ctx: Ctx {
                    repo: Repository {
                        meta: &meta,
                        blobs: &blobs,
                    },
                },
            };
            meta.gets.store(0, Ordering::Relaxed);
            assert_eq!(query.merge_bases(a, b).unwrap(), vec![id]);
            let reads = meta.gets.load(Ordering::Relaxed);
            assert!(reads <= 32, "history {n}: {reads} metadata reads");
        }
    }
}
#[test]
fn pruned_merge_bases_match_all_five_node_dags_and_tip_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
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
    // All subsets of the ten possible topologically ordered edges, including
    // disconnected roots, ancestor tips, criss-crosses and stale queued nodes.
    for mask in 0u16..1024 {
        let mut ids = Vec::new();
        let mut reach = Vec::<BTreeSet<usize>>::new();
        let mut bit = 0;
        let mut first = Vec::new();
        for n in 0..5 {
            let mut parents = Vec::new();
            let mut ancestors = BTreeSet::from([n]);
            for p in 0..n {
                if mask & (1 << bit) != 0 {
                    parents.push(ids[p]);
                    ancestors.extend(reach[p].iter().copied());
                }
                bit += 1;
            }
            first.push(
                parents
                    .first()
                    .and_then(|p| ids.iter().position(|id| id == p)),
            );
            ids.push(ops.create_changeset(&cs(parents, n as u32)).unwrap());
            reach.push(ancestors);
        }
        for a in 0..5 {
            for b in 0..5 {
                let mut current = Some(b);
                let mut expected_first = false;
                while let Some(node) = current {
                    if node == a {
                        expected_first = true;
                        break;
                    }
                    current = first[node];
                }
                assert_eq!(
                    ops.is_first_parent_ancestor(ids[a], ids[b]).unwrap(),
                    expected_first,
                    "first-parent DAG {mask}, tips {a}/{b}"
                );
                let common: BTreeSet<_> = reach[a].intersection(&reach[b]).copied().collect();
                let mut expected: Vec<_> = common
                    .iter()
                    .filter(|c| !common.iter().any(|d| d != *c && reach[*d].contains(c)))
                    .map(|c| ids[*c])
                    .collect();
                expected.sort();
                assert_eq!(
                    ops.merge_bases(ids[a], ids[b]).unwrap(),
                    expected,
                    "DAG {mask}, tips {a}/{b}"
                );
            }
        }
    }
}
