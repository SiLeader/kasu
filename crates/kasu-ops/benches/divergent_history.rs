use kasu_core::*;
use kasu_derived::*;
use kasu_ops::*;
use kasu_store::*;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

#[derive(Default)]
struct Counts {
    meta_get: AtomicU64,
    meta_put: AtomicU64,
    blob_get: AtomicU64,
    blob_put: AtomicU64,
}
impl Counts {
    fn snapshot(&self) -> [u64; 4] {
        [
            &self.meta_get,
            &self.meta_put,
            &self.blob_get,
            &self.blob_put,
        ]
        .map(|v| v.load(Ordering::Relaxed))
    }
}
struct Metadata<'a> {
    inner: &'a dyn MetaStore,
    counts: &'a Counts,
}
impl MetaStore for Metadata<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.counts.meta_get.fetch_add(1, Ordering::Relaxed);
        self.inner.get(key)
    }
    fn put_if_absent(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.counts.meta_put.fetch_add(1, Ordering::Relaxed);
        self.inner.put_if_absent(key, value)
    }
}
struct Blobs<'a> {
    inner: &'a dyn BlobStore,
    counts: &'a Counts,
}
impl BlobStore for Blobs<'_> {
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.counts.blob_get.fetch_add(1, Ordering::Relaxed);
        self.inner.get_compressed(id)
    }
    fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        self.counts.blob_put.fetch_add(1, Ordering::Relaxed);
        self.inner.put_compressed(id, bytes)
    }
}
#[derive(Default)]
struct Batch {
    time: [f64; 3],
    count: [u64; 3],
    calls: [u64; 4],
    max_meta_get: u64,
}
fn main() {
    if std::env::var_os("KASU_BENCH_DIVERGENT_HISTORY").is_none() {
        return;
    }
    let count: u32 = std::env::var("KASU_BENCH_HISTORY_COUNT")
        .unwrap_or_else(|_| "100000".into())
        .parse()
        .expect("history count must be an integer");
    assert!(
        (10..=1_000_000).contains(&count) && count.is_multiple_of(10),
        "count must be a multiple of 10 in 10..1000000"
    );
    let dir = tempfile::tempdir().unwrap();
    let database = SqliteStore::open(dir.path().join("meta.db")).unwrap();
    let filesystem = FsBlobStore::open(dir.path().join("objects")).unwrap();
    let counts = Counts::default();
    let meta = Metadata {
        inner: &database,
        counts: &counts,
    };
    let blobs = Blobs {
        inner: &filesystem,
        counts: &counts,
    };
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
    let sig = Signature {
        name: b"Bench".as_slice().into(),
        email: b"bench@example.com".as_slice().into(),
        timestamp: 0,
        tz: b"+0000".as_slice().into(),
    };
    let contents = [
        ops.ctx.repo.put_blob(b"version A\n").unwrap(),
        ops.ctx.repo.put_blob(b"version B\n").unwrap(),
    ];
    let mut main_history = Vec::<ChangesetId>::new();
    let mut side = None;
    let mut fork = None;
    let mut merges = 0;
    let mut batch = Batch::default();
    println!(
        "history_length,merges,mean_create_microseconds,main_mean_microseconds,side_mean_microseconds,merge_mean_microseconds,mean_meta_get_calls,max_meta_get_calls,mean_meta_put_calls,mean_blob_get_calls,mean_blob_put_calls"
    );
    for n in 0..count {
        let kind = match n % 10 {
            8 => 1,
            9 => 2,
            _ => 0,
        };
        let (parents, path, version) = match kind {
            1 => {
                let base = main_history[main_history.len() - 4];
                fork = Some(base);
                (vec![base], b"bench/side".as_slice(), (n / 10) % 2)
            }
            2 => (
                vec![*main_history.last().unwrap(), side.unwrap()],
                b"bench/side".as_slice(),
                (n / 10) % 2,
            ),
            _ => (
                main_history.last().copied().into_iter().collect(),
                b"bench/main".as_slice(),
                n % 2,
            ),
        };
        let cs = Changeset {
            parents,
            author: sig.clone(),
            committer: sig.clone(),
            message: n.to_be_bytes().as_slice().into(),
            git_provenance: None,
            changes: vec![FileChange::Set {
                path: RepoPath::new(path).unwrap(),
                entry: Entry::File {
                    blob: contents[version as usize],
                    executable: false,
                },
            }],
        };
        let before = counts.snapshot();
        let start = Instant::now();
        let id = ops.create_changeset(&cs).unwrap();
        let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
        let after = counts.snapshot();
        batch.time[kind] += elapsed;
        batch.count[kind] += 1;
        for i in 0..4 {
            batch.calls[i] += after[i] - before[i];
        }
        batch.max_meta_get = batch.max_meta_get.max(after[0] - before[0]);
        if kind == 1 {
            side = Some(id);
        } else {
            if kind == 2 {
                let first = cs.parents[0];
                let second = cs.parents[1];
                // Untimed: ensure these are divergent tips and their actual best merge base.
                assert!(!ops.is_ancestor(first, second).unwrap());
                assert!(!ops.is_ancestor(second, first).unwrap());
                assert_eq!(ops.merge_bases(first, second).unwrap(), vec![fork.unwrap()]);
                assert!(ops.is_ancestor(second, id).unwrap());
                assert_ne!(
                    ops.read_file(first, &RepoPath::new(b"bench/main".as_slice()).unwrap())
                        .unwrap(),
                    ops.read_file(second, &RepoPath::new(b"bench/main".as_slice()).unwrap())
                        .unwrap()
                );
                assert_eq!(
                    ops.read_file(id, &RepoPath::new(b"bench/main".as_slice()).unwrap())
                        .unwrap(),
                    ops.read_file(first, &RepoPath::new(b"bench/main".as_slice()).unwrap())
                        .unwrap()
                );
                assert_eq!(
                    ops.read_file(id, &RepoPath::new(b"bench/side".as_slice()).unwrap())
                        .unwrap(),
                    ops.read_file(second, &RepoPath::new(b"bench/side".as_slice()).unwrap())
                        .unwrap()
                );
                merges += 1;
            }
            main_history.push(id);
        }
        if (n + 1).is_multiple_of(1000) || n + 1 == count {
            let samples = batch.count.iter().sum::<u64>() as f64;
            println!(
                "{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{},{:.2},{:.2},{:.2}",
                n + 1,
                merges,
                batch.time.iter().sum::<f64>() / samples,
                batch.time[0] / batch.count[0] as f64,
                batch.time[1] / batch.count[1] as f64,
                batch.time[2] / batch.count[2] as f64,
                batch.calls[0] as f64 / samples,
                batch.max_meta_get,
                batch.calls[1] as f64 / samples,
                batch.calls[2] as f64 / samples,
                batch.calls[3] as f64 / samples
            );
            batch = Batch::default();
        }
    }
    let tip = *main_history.last().unwrap();
    assert_eq!(merges, count / 10);
    assert_eq!(main_history.len(), (count - count / 10) as usize);
    assert!(ops.is_ancestor(main_history[0], tip).unwrap());
    eprintln!(
        "history_count={count} divergent_merges={merges} tip_changeset={tip} root_tree={}",
        ops.root_tree(tip).unwrap()
    );
}
