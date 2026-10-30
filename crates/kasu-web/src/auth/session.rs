//! Stateless sessions: a signed cookie naming the account and the server's
//! binding of its PAT. Replacing the PAT makes the binding stale, so the
//! server rejects the session on its next request.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const VERSION: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    pub account: String,
    pub binding: Vec<u8>,
    pub issued: i64,
    pub last: i64,
    pub id: [u8; 16],
}
pub fn mac(key: &[u8], domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of all sizes");
    mac.update(domain);
    for part in parts {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}
impl Session {
    pub fn new(account: String, binding: Vec<u8>, now: i64) -> Self {
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).expect("the operating system provides randomness");
        Self {
            account,
            binding,
            issued: now,
            last: now,
            id,
        }
    }
    pub fn encode(&self, key: &[u8]) -> String {
        let mut payload = vec![VERSION];
        payload.extend(self.issued.to_be_bytes());
        payload.extend(self.last.to_be_bytes());
        payload.extend(self.id);
        payload.push(self.account.len() as u8);
        payload.extend(self.account.as_bytes());
        payload.push(self.binding.len() as u8);
        payload.extend(&self.binding);
        let tag = mac(key, b"kasu.session.v1", &[&payload]);
        format!("{}.{}", B64.encode(payload), B64.encode(tag))
    }
    /// Verify a cookie value and its lifetimes at `now`.
    pub fn decode(key: &[u8], value: &str, now: i64, idle: i64, max: i64) -> Option<Self> {
        let (payload, tag) = value.split_once('.')?;
        let payload = B64.decode(payload).ok()?;
        let tag = B64.decode(tag).ok()?;
        let expected = mac(key, b"kasu.session.v1", &[&payload]);
        if !constant_eq(&tag, &expected) {
            return None;
        }
        let (&version, rest) = payload.split_first()?;
        if version != VERSION || rest.len() < 34 {
            return None;
        }
        let issued = i64::from_be_bytes(rest[0..8].try_into().ok()?);
        let last = i64::from_be_bytes(rest[8..16].try_into().ok()?);
        let id: [u8; 16] = rest[16..32].try_into().ok()?;
        let rest = &rest[32..];
        let (&len, rest) = rest.split_first()?;
        let account = String::from_utf8(rest.get(..len as usize)?.to_vec()).ok()?;
        let rest = &rest[len as usize..];
        let (&len, rest) = rest.split_first()?;
        if rest.len() != len as usize || account.is_empty() {
            return None;
        }
        if now < issued || now - last > idle || now - issued > max {
            return None;
        }
        Some(Self {
            account,
            binding: rest.to_vec(),
            issued,
            last,
            id,
        })
    }
}
pub fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    #[test]
    fn sessions_expire_after_idle_and_absolute_limits() {
        let key = [1u8; 32];
        let mut session = Session::new("alice".into(), vec![9; 32], 1000);
        let cookie = session.encode(&key);
        assert_eq!(
            Session::decode(&key, &cookie, 1100, 7200, 43200),
            Some(session.clone())
        );
        assert!(Session::decode(&key, &cookie, 1000 + 7201, 7200, 43200).is_none());
        session.last = 1000 + 43000;
        let cookie = session.encode(&key);
        assert!(Session::decode(&key, &cookie, 1000 + 43100, 7200, 43200).is_some());
        assert!(Session::decode(&key, &cookie, 1000 + 43201, 7200, 43200).is_none());
        assert!(Session::decode(&[2u8; 32], &cookie, 1000 + 43100, 7200, 43200).is_none());
    }
    proptest! {
        #[test]
        fn any_tampering_is_detected(index in 0usize..200, bit in 0u8..8) {
            let key = [3u8; 32];
            let cookie = Session::new("alice".into(), vec![5; 32], 10).encode(&key);
            let mut bytes = cookie.into_bytes();
            let index = index % bytes.len();
            bytes[index] ^= 1 << bit;
            if let Ok(tampered) = String::from_utf8(bytes) {
                let decoded = Session::decode(&key, &tampered, 10, 100, 100);
                // Base64 padding bits may decode to the same payload.
                if let Some(session) = decoded {
                    prop_assert_eq!(session.encode(&key).len(), tampered.len());
                    prop_assert_eq!(session.account, "alice");
                }
            }
        }
    }
}
