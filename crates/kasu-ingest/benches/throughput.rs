use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kasu_core::*;
use kasu_derived::*;
use kasu_git::*;
use kasu_ops::Operations;
use kasu_store::*;
use std::{
    cell::Cell,
    collections::BTreeSet,
    hint::black_box,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

struct Fixture {
    dir: tempfile::TempDir,
    meta: SqliteStore,
    blobs: FsBlobStore,
    engine: Engine,
    ids: Vec<ChangesetId>,
    contents: [BlobId; 2],
    sequence: Cell<u64>,
}
impl Fixture {
    fn new(depth: usize, blob_bytes: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta = SqliteStore::memory().unwrap();
        let blobs = FsBlobStore::open(dir.path().join("objects")).unwrap();
        let repo = Repository {
            meta: &meta,
            blobs: &blobs,
        };
        // A fixed seed makes fixture content and all resulting IDs reproducible.
        let mut random = 0x0123_4567_89ab_cdef_u64;
        let a: Vec<_> = (0..blob_bytes)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                random as u8
            })
            .collect();
        let mut b = a.clone();
        b[blob_bytes / 2] ^= 1;
        let contents = [repo.put_blob(&a).unwrap(), repo.put_blob(&b).unwrap()];
        let mut engine = Engine::standard();
        register_git(&mut engine);
        let mut fixture = Self {
            dir,
            meta,
            blobs,
            engine,
            ids: Vec::new(),
            contents,
            sequence: Cell::new(0),
        };
        for n in 0..=depth {
            let mut parents: Vec<_> = fixture.ids.last().copied().into_iter().collect();
            if n >= 10 && n % 10 == 0 {
                parents.push(fixture.ids[n - 10]);
            }
            let cs = fixture.changeset(parents, n as u64, contents[n % 2]);
            fixture
                .ids
                .push(fixture.ops().create_changeset(&cs).unwrap());
        }
        fixture.sequence.set(depth as u64 + 1);
        fixture
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            repo: Repository {
                meta: &self.meta,
                blobs: &self.blobs,
            },
        }
    }
    fn ops(&self) -> Operations<'_> {
        Operations {
            engine: &self.engine,
            ctx: self.ctx(),
        }
    }
    fn tip(&self) -> ChangesetId {
        *self.ids.last().unwrap()
    }
    fn path() -> RepoPath {
        RepoPath::new(b"bench/nested/file".as_slice()).unwrap()
    }
    fn changeset(&self, parents: Vec<ChangesetId>, sequence: u64, blob: BlobId) -> Changeset {
        let signature = Signature {
            name: b"Criterion".as_slice().into(),
            email: b"bench@kasu.invalid".as_slice().into(),
            timestamp: 0,
            tz: b"+0000".as_slice().into(),
        };
        Changeset {
            parents,
            author: signature.clone(),
            committer: signature,
            message: format!("criterion commit {sequence}\n").into_bytes().into(),
            changes: vec![FileChange::Set {
                path: Self::path(),
                entry: Entry::File {
                    blob,
                    executable: false,
                },
            }],
            git_provenance: None,
        }
    }
    fn child(&self) -> Changeset {
        let sequence = self.sequence.get();
        self.sequence.set(sequence.checked_add(1).unwrap());
        // Every measured write is a unique child of the same prepared tip, and
        // changes its content. No iteration is an existing-object replay.
        self.changeset(
            vec![self.tip()],
            sequence,
            self.contents[self.ids.len() % 2],
        )
    }
}

fn changesets(c: &mut Criterion) {
    let mut group = c.benchmark_group("changeset_create");
    group.throughput(Throughput::Elements(1));
    for depth in [100, 1_000, 10_000] {
        let fixture = Fixture::new(depth, 64);
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, _| {
            b.iter_batched(
                || fixture.child(),
                |cs| black_box(fixture.ops().create_changeset(&cs).unwrap()),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn derived(c: &mut Criterion) {
    let mut group = c.benchmark_group("path_history");
    for depth in [100, 1_000] {
        group.throughput(Throughput::Elements(depth as u64 + 1));
        group.bench_with_input(BenchmarkId::new("cold", depth), &depth, |b, &depth| {
            // Each iteration has a fresh durable key space. Setup and fixture
            // destruction stay outside the measured derivation interval.
            b.iter_batched_ref(
                || Fixture::new(depth, 64),
                |fixture| {
                    let result = fixture
                        .engine
                        .derive::<PathHistory>(&fixture.ctx(), fixture.tip(), &Fixture::path())
                        .unwrap();
                    assert_eq!(result.last, Some(fixture.tip()));
                    black_box(result)
                },
                BatchSize::PerIteration,
            );
        });
        let fixture = Fixture::new(depth, 64);
        let expected = fixture
            .engine
            .derive::<PathHistory>(&fixture.ctx(), fixture.tip(), &Fixture::path())
            .unwrap();
        // A hit visits one derived record, independent of the historical depth.
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new("warm", depth), &depth, |b, _| {
            b.iter(|| {
                let result = fixture
                    .engine
                    .derive::<PathHistory>(&fixture.ctx(), fixture.tip(), &Fixture::path())
                    .unwrap();
                assert_eq!(result, expected);
                black_box(result)
            });
        });
    }
    group.finish();
}

fn git(repo: &Path, args: &[&str], input: &[u8]) {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[derive(Default)]
struct MeasuredWriter {
    bytes: usize,
}
impl Write for MeasuredWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // Consume the actual slice behind an optimization barrier, including
        // the final pack checksum, without buffering a full copy of the pack.
        let bytes = black_box(bytes);
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn packs(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack_encode");
    let mut sizes = String::from(
        "blob_bytes,commits,objects,raw_bytes,pack_bytes,index_pack_strict,fsck_strict\n",
    );
    for blob_bytes in [4 * 1024, 256 * 1024] {
        let fixture = Fixture::new(100, blob_bytes);
        let projection = ProjectionId::full(b"criterion-pack");
        let tip = fixture
            .engine
            .derive::<GitCommit>(&fixture.ctx(), fixture.tip(), &projection)
            .unwrap()
            .0;
        let index = GitIndex {
            repo: fixture.ctx().repo,
            projection,
        };
        let objects = reachable(&index, [tip], &BTreeSet::new()).unwrap();
        let raw_bytes = objects
            .iter()
            .map(|oid| index.object(*oid).unwrap().1.len() as u64)
            .sum::<u64>();
        let mut cold = Vec::new();
        write_pack(&index, objects.iter().copied(), &mut cold).unwrap();
        let cache = PackCache::new(16 * 1024 * 1024);
        let mut warm = Vec::new();
        write_pack_cached(&index, objects.iter().copied(), Some(&cache), &mut warm).unwrap();
        assert_eq!(warm, cold);
        warm.clear();
        write_pack_cached(&index, objects.iter().copied(), Some(&cache), &mut warm).unwrap();
        assert_eq!(warm, cold);
        assert!(cache.stats().hits >= objects.len() as u64);
        // Validate the fixed input/output before timing. Neither Git subprocesses
        // nor revision enumeration are included in encoding throughput.
        let repo = fixture.dir.path().join("validation.git");
        std::fs::create_dir(&repo).unwrap();
        git(
            &repo,
            &[
                "init",
                "--bare",
                "--template=",
                "--object-format=sha1",
                "--initial-branch=main",
            ],
            b"",
        );
        git(&repo, &["index-pack", "--stdin", "--strict"], &cold);
        git(
            &repo,
            &["update-ref", "refs/heads/main", &tip.to_string()],
            b"",
        );
        git(&repo, &["fsck", "--strict"], b"");
        use std::fmt::Write as _;
        writeln!(
            sizes,
            "{blob_bytes},{},{},{raw_bytes},{},true,true",
            fixture.ids.len(),
            objects.len(),
            cold.len()
        )
        .unwrap();
        group.throughput(Throughput::Bytes(raw_bytes));
        group.bench_with_input(BenchmarkId::new("cold", blob_bytes), &blob_bytes, |b, _| {
            b.iter(|| {
                let mut out = MeasuredWriter::default();
                write_pack(&index, objects.iter().copied(), &mut out).unwrap();
                assert_eq!(out.bytes, cold.len());
                black_box(out.bytes)
            });
        });
        group.bench_with_input(BenchmarkId::new("warm", blob_bytes), &blob_bytes, |b, _| {
            b.iter(|| {
                let mut out = MeasuredWriter::default();
                write_pack_cached(&index, objects.iter().copied(), Some(&cache), &mut out).unwrap();
                assert_eq!(out.bytes, cold.len());
                black_box(out.bytes)
            });
        });
        assert!(cache.stats().hits >= objects.len() as u64);
    }
    group.finish();
    eprintln!("{sizes}");
    if let Some(directory) = std::env::var_os("KASU_CRITERION_OUTPUT") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("pack-sizes.csv"), sizes).unwrap();
    }
}

criterion_group! { name = benches; config = Criterion::default().sample_size(20).warm_up_time(Duration::from_millis(200)).measurement_time(Duration::from_secs(1)).without_plots(); targets = changesets, derived, packs }
criterion_main!(benches);
