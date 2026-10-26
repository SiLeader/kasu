use kasu_core::*;
use kasu_derived::*;
use kasu_ops::*;
use kasu_store::*;
use std::time::Instant;

// Run: KASU_BENCH_HISTORY=1 RUSTC_WRAPPER= cargo bench -p kasu-ops --bench history
// Samples identical changeset writes at increasing history lengths. Raw measurements
// are emitted rather than imposing a machine-dependent timing assertion.
fn main() {
    if std::env::var_os("KASU_BENCH_HISTORY").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::open(dir.path().join("meta.db")).unwrap();
    let blobs = FsBlobStore::open(dir.path().join("objects")).unwrap();
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
    let path = RepoPath::new(b"bench/file".as_slice()).unwrap();
    let mut ids = Vec::new();
    let mut batch = Instant::now();
    println!("history_length,mean_create_microseconds,merges");
    for n in 0u32..100_000 {
        let mut parents: Vec<_> = ids.last().copied().into_iter().collect();
        if n >= 10 && n % 10 == 0 {
            parents.push(ids[n as usize - 10]);
        }
        let cs = Changeset {
            parents,
            author: sig.clone(),
            committer: sig.clone(),
            message: n.to_be_bytes().as_slice().into(),
            changes: vec![FileChange::Set {
                path: path.clone(),
                entry: Entry::File {
                    blob: contents[n as usize % 2],
                    executable: false,
                },
            }],
            git_provenance: None,
        };
        ids.push(ops.create_changeset(&cs).unwrap());
        if (n + 1) % 1000 == 0 {
            println!(
                "{},{:.2},{}",
                n + 1,
                batch.elapsed().as_secs_f64() * 1000.0,
                n / 10
            );
            batch = Instant::now();
        }
    }
    assert!(ops.is_ancestor(ids[0], *ids.last().unwrap()).unwrap());
}
