use kasu_core::*;
use kasu_derived::*;
use kasu_store::*;
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};

static CALLS: AtomicUsize = AtomicUsize::new(0);
#[derive(Debug, PartialEq)]
struct Probe<const V: u32>(u64);
impl<const V: u32> Canonical for Probe<V> {
    fn encode_to(&self, out: &mut Encoder) {
        out.u64(self.0);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(input.u64()?))
    }
}
impl<const V: u32> DerivedData for Probe<V> {
    const NAME: &'static str = "Probe";
    const VERSION: u32 = V;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::None;
    type SubKey = RepoPath;
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, _: &RepoPath) -> Result<Vec<DerivedRef>> {
        Ok(vec![DerivedRef::of::<Generation>(cs.id(), &())])
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, path: &RepoPath, _: &[Self]) -> Result<Self> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        if V == 0 {
            return Err(invalid("probe computation failed"));
        }
        Ok(Self(
            ctx.get::<Generation>(cs.id(), &())?.0 + path.as_bytes().len() as u64 + V as u64,
        ))
    }
}
fn commit() -> Changeset {
    let sig = Signature {
        name: b"A".as_slice().into(),
        email: b"a@b".as_slice().into(),
        timestamp: 0,
        tz: b"+0000".as_slice().into(),
    };
    Changeset {
        parents: vec![],
        author: sig.clone(),
        committer: sig,
        message: BString::default(),
        changes: vec![],
        git_provenance: None,
    }
}
#[test]
fn lazy_keys_versions_dependencies_and_concurrent_singleflight() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
    };
    let id = ctx.repo.put_changeset(&commit()).unwrap();
    let mut engine = Engine::standard();
    engine.register::<Probe<1>>();
    engine.register::<Probe<2>>();
    let path = RepoPath::new(b"a/\xff".as_slice()).unwrap();
    let other = RepoPath::new(b"b".as_slice()).unwrap();
    engine.eager(&ctx, id).unwrap();
    assert!(ctx.get::<Probe<1>>(id, &path).is_err());
    let barrier = Barrier::new(32);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..32)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    engine.derive::<Probe<1>>(&ctx, id, &path).unwrap()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), Probe(5));
        }
    });
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    assert!(ctx.get::<Probe<1>>(id, &other).is_err());
    assert!(ctx.get::<Probe<2>>(id, &path).is_err());
    assert_eq!(
        engine.derive::<Probe<2>>(&ctx, id, &path).unwrap(),
        Probe(6)
    );
    assert_eq!(ctx.get::<Probe<1>>(id, &path).unwrap(), Probe(5));
    engine.register::<Probe<0>>();
    assert!(engine.derive::<Probe<0>>(&ctx, id, &path).is_err());
    assert!(ctx.get::<Probe<0>>(id, &path).is_err());
    // A failed derivation remains retryable and records each failed computation.
    assert!(engine.derive::<Probe<0>>(&ctx, id, &path).is_err());
    let metrics = engine.metrics();
    assert!(
        metrics.contains("kasu_derived_compute_success_total{name=\"Probe\",version=\"1\"} 1\n")
    );
    assert!(
        metrics.contains("kasu_derived_compute_errors_total{name=\"Probe\",version=\"0\"} 2\n")
    );
    assert!(
        metrics.contains("kasu_derived_compute_seconds_count{name=\"Probe\",version=\"0\"} 2\n")
    );
    assert!(metrics.contains(
        "kasu_derived_compute_seconds_bucket{name=\"Probe\",version=\"0\",le=\"+Inf\"} 2\n"
    ));
    assert!(!metrics.contains("changeset="));
    assert!(!metrics.contains("subkey="));
    for line in metrics
        .lines()
        .filter(|line| line.starts_with("kasu_derived_compute_active"))
    {
        assert!(line.ends_with(" 0"), "{line}");
    }
}

struct Cycle;
impl Canonical for Cycle {
    fn encode_to(&self, _: &mut Encoder) {}
    fn decode_from(_: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self)
    }
}
impl DerivedData for Cycle {
    const NAME: &'static str = "Cycle";
    const VERSION: u32 = 1;
    const MODE: DeriveMode = DeriveMode::Lazy;
    const PARENTS: ParentSelection = ParentSelection::None;
    type SubKey = ();
    fn dependencies(_: &Ctx<'_>, cs: &Changeset, _: &()) -> Result<Vec<DerivedRef>> {
        Ok(vec![DerivedRef::of::<Self>(cs.id(), &())])
    }
    fn derive(_: &Ctx<'_>, _: &Changeset, _: &(), _: &[Self]) -> Result<Self> {
        panic!("cycle must be rejected before computation")
    }
}
#[test]
fn cycles_and_unregistered_types_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let meta = SqliteStore::memory().unwrap();
    let blobs = FsBlobStore::open(dir.path()).unwrap();
    let ctx = Ctx {
        repo: Repository {
            meta: &meta,
            blobs: &blobs,
        },
    };
    let id = ctx.repo.put_changeset(&commit()).unwrap();
    let mut engine = Engine::standard();
    assert!(engine.derive::<Cycle>(&ctx, id, &()).is_err());
    engine.register::<Cycle>();
    assert!(matches!(
        engine.derive::<Cycle>(&ctx, id, &()),
        Err(Error::Invalid(_))
    ));
}
