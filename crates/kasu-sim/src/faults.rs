use kasu_core::Result;
use kasu_store::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy)]
pub enum Fault {
    BeforeCommit,
    LostReply,
}
pub struct FaultStore {
    pub inner: Arc<SqliteStore>,
    faults: Mutex<BTreeMap<Vec<u8>, Fault>>,
    pub failures: AtomicU64,
    pub lost_replies: AtomicU64,
    gate: Mutex<Option<Arc<CommitGate>>>,
}
#[derive(Default)]
pub struct CommitGate {
    state: Mutex<(bool, bool, bool)>,
    changed: Condvar,
}
impl CommitGate {
    fn arrive(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
    }
    fn finish(&self) {
        self.state.lock().unwrap().2 = true;
        self.changed.notify_all();
    }
    pub fn wait_reached(&self) {
        self.wait_for(|s| s.0);
    }
    pub fn wait_finished(&self) {
        self.wait_for(|s| s.2);
    }
    fn wait_for(&self, predicate: impl Fn(&(bool, bool, bool)) -> bool) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, std::time::Duration::from_secs(5), |s| !predicate(s))
            .unwrap();
        assert!(predicate(&state), "commit gate timed out");
    }
    pub fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}
impl FaultStore {
    pub fn new(inner: Arc<SqliteStore>) -> Self {
        Self {
            inner,
            faults: Mutex::new(BTreeMap::new()),
            failures: AtomicU64::new(0),
            lost_replies: AtomicU64::new(0),
            gate: Mutex::new(None),
        }
    }
    pub fn inject(&self, key: Vec<u8>, fault: Fault) {
        self.faults.lock().unwrap().insert(key, fault);
    }
    pub fn pause_next_commit(&self) -> Arc<CommitGate> {
        let gate = Arc::new(CommitGate::default());
        *self.gate.lock().unwrap() = Some(gate.clone());
        gate
    }
}
impl RefStore for FaultStore {
    fn capabilities(&self) -> RefCapabilities {
        self.inner.capabilities()
    }
    fn resolve(&self, r: &[u8], n: &[u8]) -> Result<RefState> {
        self.inner.resolve(r, n)
    }
    fn list(&self, r: &[u8], p: &[u8]) -> Result<Vec<(Vec<u8>, RefState)>> {
        self.inner.list(r, p)
    }
    fn compare_and_swap(&self, r: &[u8], u: &[RefUpdate], a: &[u8], t: i64) -> Result<CasResult> {
        self.inner.compare_and_swap(r, u, a, t)
    }
    fn tail(&self, r: &[u8], s: u64, l: usize) -> Result<Vec<RefLog>> {
        self.inner.tail(r, s, l)
    }
    fn land_record(&self, r: &[u8], k: &[u8]) -> Result<Option<LandRecord>> {
        self.inner.land_record(r, k)
    }
    fn commit_land(
        &self,
        r: &[u8],
        k: &[u8],
        c: &LandCommit,
        a: &[u8],
        t: i64,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<LandCommitResult> {
        let fault = self.faults.lock().unwrap().get(k).copied();
        if matches!(fault, Some(Fault::BeforeCommit)) {
            self.faults.lock().unwrap().remove(k);
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Err(kasu_store::storage_error(
                "injected storage failure before commit",
            ));
        }
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = &gate {
            gate.arrive();
        }
        let result = self.inner.commit_land(r, k, c, a, t, cancel);
        if let Some(gate) = gate {
            gate.finish();
        }
        let result = result?;
        if matches!(fault, Some(Fault::LostReply))
            && matches!(
                result,
                LandCommitResult::Recorded {
                    replayed: false,
                    ..
                }
            )
        {
            self.faults.lock().unwrap().remove(k);
            self.lost_replies.fetch_add(1, Ordering::Relaxed);
            return Err(kasu_store::storage_error(
                "injected lost reply after durable commit",
            ));
        }
        Ok(result)
    }
}
impl GitRefStore for FaultStore {
    fn compare_native_and_git(
        &self,
        r: &[u8],
        n: &[RefUpdate],
        g: &[GitRefUpdate],
        a: &[u8],
        t: i64,
    ) -> Result<RefPublicationResult> {
        self.inner.compare_native_and_git(r, n, g, a, t)
    }
    fn resolve_git(&self, r: &[u8], n: &[u8]) -> Result<GitRefState> {
        self.inner.resolve_git(r, n)
    }
    fn list_git(&self, r: &[u8], p: &[u8]) -> Result<Vec<(Vec<u8>, GitRefState)>> {
        self.inner.list_git(r, p)
    }
    fn compare_git(&self, r: &[u8], u: &[GitRefUpdate], a: &[u8], t: i64) -> Result<GitCasResult> {
        self.inner.compare_git(r, u, a, t)
    }
    fn tail_git(&self, r: &[u8], s: u64, l: usize) -> Result<Vec<GitRefLog>> {
        self.inner.tail_git(r, s, l)
    }
}
