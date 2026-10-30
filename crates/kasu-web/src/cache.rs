//! In-process cache of immutable upstream responses (addressed by Changeset
//! or object ID). Keys always include the account the request acted as, so
//! one account's filtered view never reaches another.
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

pub struct Cache {
    capacity: usize,
    inner: Mutex<Inner>,
}
#[derive(Default)]
struct Inner {
    entries: HashMap<Vec<u8>, Vec<u8>>,
    order: VecDeque<Vec<u8>>,
    bytes: usize,
}
impl Cache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner::default()),
        }
    }
    pub fn key(account: Option<&str>, operation: &str, parts: &[&[u8]]) -> Vec<u8> {
        let mut hasher = blake3::Hasher::new();
        for part in [
            account.unwrap_or("\0anonymous").as_bytes(),
            operation.as_bytes(),
        ]
        .into_iter()
        .chain(parts.iter().copied())
        {
            hasher.update(&(part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        hasher.finalize().as_bytes().to_vec()
    }
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.lock().ok()?.entries.get(key).cloned()
    }
    pub fn put(&self, key: Vec<u8>, value: Vec<u8>) {
        let size = key.len() + value.len();
        if size > self.capacity / 8 {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.entries.contains_key(&key) {
            return;
        }
        while inner.bytes + size > self.capacity {
            let Some(old) = inner.order.pop_front() else {
                break;
            };
            if let Some(value) = inner.entries.remove(&old) {
                inner.bytes -= old.len() + value.len();
            }
        }
        inner.bytes += size;
        inner.order.push_back(key.clone());
        inner.entries.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_separate_accounts_and_capacity_is_bounded() {
        let cache = Cache::new(1000);
        let a = Cache::key(Some("a"), "read", &[b"x"]);
        let b = Cache::key(Some("b"), "read", &[b"x"]);
        let anonymous = Cache::key(None, "read", &[b"x"]);
        assert_ne!(a, b);
        assert_ne!(a, anonymous);
        cache.put(a.clone(), vec![1; 50]);
        assert_eq!(cache.get(&a), Some(vec![1; 50]));
        assert_eq!(cache.get(&b), None);
        for i in 0..100u8 {
            cache.put(Cache::key(None, "x", &[&[i]]), vec![0; 50]);
        }
        assert!(cache.get(&a).is_none());
    }
}
