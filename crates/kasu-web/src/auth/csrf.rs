//! CSRF tokens bound to the session (or proxy user, or a random cookie for
//! visitors), the form kind and the issue time.
use super::session::{constant_eq, mac};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};

pub const LIFETIME: i64 = 2 * 3600;

pub fn token(key: &[u8], subject: &str, kind: &str, now: i64) -> String {
    let issued = now.to_be_bytes();
    let tag = mac(
        key,
        b"kasu.csrf.v1",
        &[subject.as_bytes(), kind.as_bytes(), &issued],
    );
    let mut out = issued.to_vec();
    out.extend(&tag[..16]);
    B64.encode(out)
}
pub fn verify(key: &[u8], subject: &str, kind: &str, value: &str, now: i64) -> bool {
    let Ok(raw) = B64.decode(value) else {
        return false;
    };
    if raw.len() != 24 {
        return false;
    }
    let issued: [u8; 8] = raw[..8].try_into().expect("eight bytes");
    let at = i64::from_be_bytes(issued);
    if at > now + 60 || now - at > LIFETIME {
        return false;
    }
    let tag = mac(
        key,
        b"kasu.csrf.v1",
        &[subject.as_bytes(), kind.as_bytes(), &issued],
    );
    constant_eq(&raw[8..], &tag[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_bind_subject_kind_and_time() {
        let key = [7u8; 32];
        let value = token(&key, "s1", "edit", 100);
        assert!(verify(&key, "s1", "edit", &value, 200));
        assert!(!verify(&key, "s2", "edit", &value, 200));
        assert!(!verify(&key, "s1", "delete", &value, 200));
        assert!(!verify(&key, "s1", "edit", &value, 100 + LIFETIME + 1));
        assert!(!verify(&[8u8; 32], "s1", "edit", &value, 200));
        assert!(!verify(&key, "s1", "edit", "garbage", 200));
    }
}
