use kasu_core::{Canonical, Changeset, ChangesetId, Error, Result, invalid};
use kasu_store::{Repository, storage_error};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeriveMode {
    Eager,
    Lazy,
}
#[derive(Clone, Copy, Debug)]
pub enum ParentSelection {
    None,
    FirstParent,
    AllParents,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DerivedRef {
    pub name: String,
    pub version: u32,
    pub changeset: ChangesetId,
    pub subkey: Vec<u8>,
}
impl DerivedRef {
    pub fn of<T: DerivedData>(changeset: ChangesetId, key: &T::SubKey) -> Self {
        Self {
            name: T::NAME.into(),
            version: T::VERSION,
            changeset,
            subkey: key.encode(),
        }
    }
    pub fn storage_key(&self) -> Vec<u8> {
        // Hex keeps arbitrary SubKey bytes unambiguous in the key namespace.
        let mut key =
            format!("derived/{}@{}/{}/", self.name, self.version, self.changeset).into_bytes();
        for byte in &self.subkey {
            key.extend(format!("{byte:02x}").bytes());
        }
        key
    }
}
pub struct Ctx<'a> {
    pub repo: Repository<'a>,
}
impl Ctx<'_> {
    pub fn get<T: DerivedData>(&self, cs: ChangesetId, key: &T::SubKey) -> Result<T> {
        let reference = DerivedRef::of::<T>(cs, key);
        let raw = self
            .repo
            .meta
            .get(&reference.storage_key())?
            .ok_or_else(|| Error::NotFound(format!("derived {}", reference.name)))?;
        T::decode(&raw)
    }
}
pub trait DerivedData: Canonical + Send + Sync + 'static {
    const NAME: &'static str;
    const VERSION: u32;
    const MODE: DeriveMode;
    const PARENTS: ParentSelection;
    type SubKey: Canonical;
    fn eager_keys(_ctx: &Ctx<'_>, _cs: &Changeset) -> Result<Vec<Self::SubKey>> {
        Ok(Vec::new())
    }
    fn dependencies(
        _ctx: &Ctx<'_>,
        _cs: &Changeset,
        _key: &Self::SubKey,
    ) -> Result<Vec<DerivedRef>> {
        Ok(Vec::new())
    }
    fn derive(ctx: &Ctx<'_>, cs: &Changeset, key: &Self::SubKey, parents: &[Self]) -> Result<Self>;
    // Called only after regular dependencies are durable. This permits a path
    // lineage to select a different parent SubKey without recursive derivation.
    fn resolved_dependencies(
        _ctx: &Ctx<'_>,
        _cs: &Changeset,
        _key: &Self::SubKey,
    ) -> Result<Vec<DerivedRef>> {
        Ok(Vec::new())
    }
}
trait Erased: Send + Sync {
    fn eager_keys(&self, ctx: &Ctx<'_>, cs: &Changeset) -> Result<Vec<Vec<u8>>>;
    fn dependencies(
        &self,
        ctx: &Ctx<'_>,
        cs: &Changeset,
        reference: &DerivedRef,
    ) -> Result<Vec<DerivedRef>>;
    fn derive(&self, ctx: &Ctx<'_>, cs: &Changeset, reference: &DerivedRef) -> Result<Vec<u8>>;
    fn resolved_dependencies(
        &self,
        ctx: &Ctx<'_>,
        cs: &Changeset,
        reference: &DerivedRef,
    ) -> Result<Vec<DerivedRef>>;
}
struct Adapter<T>(std::marker::PhantomData<T>);
fn parents<T: DerivedData>(cs: &Changeset) -> &[ChangesetId] {
    match T::PARENTS {
        ParentSelection::None => &[],
        ParentSelection::FirstParent => &cs.parents[..cs.parents.len().min(1)],
        ParentSelection::AllParents => &cs.parents,
    }
}
impl<T: DerivedData> Erased for Adapter<T> {
    fn resolved_dependencies(
        &self,
        ctx: &Ctx<'_>,
        cs: &Changeset,
        reference: &DerivedRef,
    ) -> Result<Vec<DerivedRef>> {
        T::resolved_dependencies(ctx, cs, &T::SubKey::decode(&reference.subkey)?)
    }
    fn eager_keys(&self, ctx: &Ctx<'_>, cs: &Changeset) -> Result<Vec<Vec<u8>>> {
        if T::MODE == DeriveMode::Lazy {
            return Ok(Vec::new());
        }
        Ok(T::eager_keys(ctx, cs)?
            .iter()
            .map(Canonical::encode)
            .collect())
    }
    fn dependencies(
        &self,
        ctx: &Ctx<'_>,
        cs: &Changeset,
        reference: &DerivedRef,
    ) -> Result<Vec<DerivedRef>> {
        let key = T::SubKey::decode(&reference.subkey)?;
        let mut deps = T::dependencies(ctx, cs, &key)?;
        deps.extend(
            parents::<T>(cs)
                .iter()
                .map(|id| DerivedRef::of::<T>(*id, &key)),
        );
        Ok(deps)
    }
    fn derive(&self, ctx: &Ctx<'_>, cs: &Changeset, reference: &DerivedRef) -> Result<Vec<u8>> {
        let key = T::SubKey::decode(&reference.subkey)?;
        let parent_values = parents::<T>(cs)
            .iter()
            .map(|id| ctx.get::<T>(*id, &key))
            .collect::<Result<Vec<_>>>()?;
        Ok(T::derive(ctx, cs, &key, &parent_values)?.encode())
    }
}
#[derive(Default)]
struct DeriveStats {
    lookups: AtomicU64,
    hits: AtomicU64,
    success: AtomicU64,
    errors: AtomicU64,
    active: AtomicU64,
    nanos: AtomicU64,
    buckets: [AtomicU64; 9],
}
const LATENCY: [u64; 9] = [
    1_000_000,
    5_000_000,
    10_000_000,
    50_000_000,
    100_000_000,
    500_000_000,
    1_000_000_000,
    5_000_000_000,
    u64::MAX,
];
impl DeriveStats {
    fn lookup(&self, present: bool) {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        if present {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn computed(&self, start: Instant, success: bool) {
        self.active.fetch_sub(1, Ordering::Relaxed);
        if success { &self.success } else { &self.errors }.fetch_add(1, Ordering::Relaxed);
        let elapsed = start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.nanos.fetch_add(elapsed, Ordering::Relaxed);
        for (bound, bucket) in LATENCY.iter().zip(&self.buckets) {
            if elapsed <= *bound {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
#[derive(Default)]
pub struct Engine {
    registry: BTreeMap<(String, u32), Box<dyn Erased>>,
    flights: Mutex<BTreeMap<DerivedRef, Weak<Mutex<()>>>>,
    stats: BTreeMap<(String, u32), DeriveStats>,
}
impl Engine {
    pub fn standard() -> Self {
        let mut engine = Self::default();
        engine.register::<crate::RootTree>();
        engine.register::<crate::Generation>();
        engine.register::<crate::FirstParentSkipV1>();
        engine.register::<crate::FirstParentSkip>();
        engine.register::<crate::PathHistory>();
        engine.register::<crate::Blame>();
        engine.register::<crate::CopyBlame>();
        engine.register::<crate::MovedBlame>();
        engine.register::<crate::LineMatches>();
        engine.register::<crate::PathIndex>();
        engine.register::<crate::CopyDetect>();
        engine
    }
    pub fn register<T: DerivedData>(&mut self) {
        assert!(!T::NAME.is_empty() && !T::NAME.contains(['/', '@']));
        self.stats.entry((T::NAME.into(), T::VERSION)).or_default();
        self.registry.insert(
            (T::NAME.into(), T::VERSION),
            Box::new(Adapter::<T>(std::marker::PhantomData)),
        );
    }
    /// Process-local Prometheus counters; labels contain registered type and version only.
    pub fn metrics(&self) -> String {
        use std::fmt::Write;
        let mut text = String::from(concat!(
            "# TYPE kasu_derived_cache_lookups_total counter\n",
            "# TYPE kasu_derived_cache_hits_total counter\n",
            "# TYPE kasu_derived_compute_success_total counter\n",
            "# TYPE kasu_derived_compute_errors_total counter\n",
            "# TYPE kasu_derived_compute_active gauge\n",
            "# TYPE kasu_derived_compute_seconds histogram\n",
        ));
        for ((name, version), s) in &self.stats {
            let name = name
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            let label = format!("name=\"{name}\",version=\"{version}\"");
            for (metric, value) in [
                ("cache_lookups_total", s.lookups.load(Ordering::Relaxed)),
                ("cache_hits_total", s.hits.load(Ordering::Relaxed)),
                ("compute_success_total", s.success.load(Ordering::Relaxed)),
                ("compute_errors_total", s.errors.load(Ordering::Relaxed)),
                ("compute_active", s.active.load(Ordering::Relaxed)),
            ] {
                let _ = writeln!(text, "kasu_derived_{metric}{{{label}}} {value}");
            }
            let count = s.success.load(Ordering::Relaxed) + s.errors.load(Ordering::Relaxed);
            let _ = writeln!(
                text,
                "kasu_derived_compute_seconds_count{{{label}}} {count}"
            );
            let _ = writeln!(
                text,
                "kasu_derived_compute_seconds_sum{{{label}}} {}",
                s.nanos.load(Ordering::Relaxed) as f64 / 1e9
            );
            for (bound, bucket) in LATENCY.iter().zip(&s.buckets) {
                let le = if *bound == u64::MAX {
                    "+Inf".into()
                } else {
                    format!("{}", *bound as f64 / 1e9)
                };
                let _ = writeln!(
                    text,
                    "kasu_derived_compute_seconds_bucket{{{label},le=\"{le}\"}} {}",
                    bucket.load(Ordering::Relaxed)
                );
            }
        }
        text
    }
    pub fn derive<T: DerivedData>(
        &self,
        ctx: &Ctx<'_>,
        cs: ChangesetId,
        key: &T::SubKey,
    ) -> Result<T> {
        self.ensure(ctx, DerivedRef::of::<T>(cs, key))?;
        ctx.get::<T>(cs, key)
    }
    pub fn eager(&self, ctx: &Ctx<'_>, cs: ChangesetId) -> Result<()> {
        let changeset = ctx.repo.changeset(cs)?;
        for ((name, version), deriver) in &self.registry {
            for subkey in deriver.eager_keys(ctx, &changeset)? {
                self.ensure(
                    ctx,
                    DerivedRef {
                        name: name.clone(),
                        version: *version,
                        changeset: cs,
                        subkey,
                    },
                )?;
            }
        }
        Ok(())
    }
    pub fn ensure(&self, ctx: &Ctx<'_>, root: DerivedRef) -> Result<()> {
        let mut stack = vec![(root, 0)];
        let mut active = BTreeSet::new();
        while let Some((reference, expanded)) = stack.pop() {
            let key = reference.storage_key();
            let stats = self.stats.get(&(reference.name.clone(), reference.version));
            let present = ctx.repo.meta.get(&key)?.is_some();
            if let Some(stats) = stats {
                stats.lookup(present);
            }
            if present {
                active.remove(&reference);
                continue;
            }
            let deriver = self
                .registry
                .get(&(reference.name.clone(), reference.version))
                .ok_or_else(|| {
                    invalid(format!(
                        "unregistered derived type {}@{}",
                        reference.name, reference.version
                    ))
                })?;
            let cs = ctx.repo.changeset(reference.changeset)?;
            if expanded == 0 {
                if !active.insert(reference.clone()) {
                    return Err(invalid("derived dependency cycle"));
                }
                let deps = deriver.dependencies(ctx, &cs, &reference)?;
                stack.push((reference, 1));
                stack.extend(deps.into_iter().rev().map(|dep| (dep, 0)));
            } else if expanded == 1 {
                let deps = deriver.resolved_dependencies(ctx, &cs, &reference)?;
                stack.push((reference, 2));
                stack.extend(deps.into_iter().rev().map(|dep| (dep, 0)));
            } else {
                let flight = {
                    let mut flights = self.flights.lock().map_err(storage_error)?;
                    // Do not retain one lock allocation per historical changeset.
                    flights.retain(|_, value| value.strong_count() > 0);
                    if let Some(flight) = flights.get(&reference).and_then(Weak::upgrade) {
                        flight
                    } else {
                        let flight = Arc::new(Mutex::new(()));
                        flights.insert(reference.clone(), Arc::downgrade(&flight));
                        flight
                    }
                };
                let _guard = flight.lock().map_err(storage_error)?;
                let stats = stats.expect("registered derived metrics");
                let present = ctx.repo.meta.get(&key)?.is_some();
                stats.lookup(present);
                if !present {
                    let start = Instant::now();
                    stats.active.fetch_add(1, Ordering::Relaxed);
                    let result = (|| {
                        let raw = deriver.derive(ctx, &cs, &reference)?;
                        ctx.repo.meta.put_if_absent(&key, &raw)?;
                        Ok::<_, Error>(())
                    })();
                    stats.computed(start, result.is_ok());
                    result?;
                    tracing::trace!(name = reference.name, changeset = %reference.changeset, "derived data computed");
                }
                active.remove(&reference);
            }
        }
        Ok(())
    }
}
