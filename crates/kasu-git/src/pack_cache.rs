use kasu_core::{GitKind, GitOid, ObjectId};
use std::sync::{Arc, Mutex};
#[derive(Clone)]
pub(crate) struct CompressedObject {
    pub kind: GitKind,
    pub size: usize,
    pub zlib: Vec<u8>,
}
#[derive(Default)]
struct State {
    objects: hashlink::LinkedHashMap<(GitOid, ObjectId), Arc<CompressedObject>>,
    bytes: usize,
    hits: u64,
    misses: u64,
}
pub struct PackCache {
    capacity: usize,
    state: Mutex<State>,
}
#[derive(Clone, Copy, Debug)]
pub struct PackCacheStats {
    pub bytes: usize,
    pub objects: usize,
    pub hits: u64,
    pub misses: u64,
}
impl PackCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(State::default()),
        }
    }
    pub fn stats(&self) -> PackCacheStats {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        PackCacheStats {
            bytes: state.bytes,
            objects: state.objects.len(),
            hits: state.hits,
            misses: state.misses,
        }
    }
    pub(crate) fn get(&self, oid: GitOid, raw: ObjectId) -> Option<Arc<CompressedObject>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = state.objects.to_back(&(oid, raw)).cloned();
        if result.is_some() {
            state.hits += 1;
        } else {
            state.misses += 1;
        }
        result
    }
    pub(crate) fn insert(&self, oid: GitOid, raw: ObjectId, object: Arc<CompressedObject>) {
        let size = object.zlib.len() + 256;
        if size > self.capacity {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.objects.contains_key(&(oid, raw)) {
            return;
        }
        while state.bytes + size > self.capacity {
            let Some((_, object)) = state.objects.pop_front() else {
                break;
            };
            state.bytes -= object.zlib.len() + 256;
        }
        state.bytes += size;
        state.objects.insert((oid, raw), object);
    }
}
