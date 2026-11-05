//! Normalised server identifiers used as credential lookup keys.

use crate::{Error, Result};
use std::fmt;

/// A normalised server identifier (`scheme://host:port`, or `unix://<path>`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerKey(String);

impl ServerKey {
    /// Normalises a server URL. Paths, queries and fragments of HTTP(S) URLs
    /// are dropped; URLs carrying userinfo are rejected.
    pub fn parse(input: &str) -> Result<Self> {
        let bad = |why: &str| Error::InvalidKey(format!("{why}: {input:?}"));
        if input.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(bad("server URL contains whitespace or control characters"));
        }
        if let Some(path) = input.strip_prefix("unix://") {
            if !path.starts_with('/') || path.len() < 2 {
                return Err(bad("unix socket path must be absolute"));
            }
            return Ok(Self(format!("unix://{path}")));
        }
        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| bad("server URL has no scheme"))?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return Err(bad("unsupported scheme")),
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return Err(bad("server URL must not contain credentials"));
        }
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, tail) = inner
                .split_once(']')
                .ok_or_else(|| bad("unterminated IPv6 literal"))?;
            if host.is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
            {
                return Err(bad("invalid IPv6 literal"));
            }
            let port = match tail {
                "" => None,
                t => Some(
                    t.strip_prefix(':')
                        .ok_or_else(|| bad("invalid authority"))?,
                ),
            };
            (format!("[{}]", host.to_ascii_lowercase()), port)
        } else {
            let (host, port) = match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            };
            if host.is_empty()
                || !host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
            {
                return Err(bad("invalid host"));
            }
            (host.to_ascii_lowercase(), port)
        };
        let port = match port {
            None | Some("") => default_port,
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| bad("invalid port"))?,
            Some(_) => return Err(bad("invalid port")),
        };
        Ok(Self(format!("{scheme}://{host}:{port}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Splits into `(scheme, rest)` where `rest` is `host:port` or the socket path.
    pub fn parts(&self) -> (&str, &str) {
        self.0
            .split_once("://")
            .expect("a normalised key has a scheme")
    }
}

impl fmt::Display for ServerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::ServerKey;
    use proptest::prelude::*;

    fn key(input: &str) -> String {
        ServerKey::parse(input).unwrap().to_string()
    }

    #[test]
    fn spellings_of_one_server_share_a_key() {
        assert_eq!(
            key("https://Kasu.Example.com/"),
            "https://kasu.example.com:443"
        );
        assert_eq!(
            key("HTTPS://kasu.example.com:443/a/b?c#d"),
            "https://kasu.example.com:443"
        );
        assert_eq!(key("http://127.0.0.1:50051"), "http://127.0.0.1:50051");
        assert_eq!(key("http://kasu"), "http://kasu:80");
        assert_eq!(key("https://[::1]:8443/path"), "https://[::1]:8443");
        assert_eq!(
            key("unix:///run/kasu/node.sock"),
            "unix:///run/kasu/node.sock"
        );
    }

    #[test]
    fn unusable_servers_are_rejected() {
        for bad in [
            "",
            "kasu.example.com",
            "ftp://x",
            "https://user:pw@kasu.example.com",
            "https://@kasu.example.com",
            "https://host:0",
            "https://host:+80",
            "https://host:99999",
            "https://ho st",
            "https://",
            "https://[::1",
            "https://host/\n",
            "unix://relative",
            "unix://",
        ] {
            assert!(ServerKey::parse(bad).is_err(), "{bad:?}");
        }
    }

    proptest! {
        #[test]
        fn normalisation_is_idempotent(input in "\\PC{0,40}") {
            if let Ok(first) = ServerKey::parse(&input) {
                prop_assert_eq!(ServerKey::parse(first.as_str()).unwrap(), first);
            }
        }

        #[test]
        fn host_case_and_default_port_do_not_matter(host in "[a-zA-Z][a-zA-Z0-9.-]{0,20}") {
            let a = ServerKey::parse(&format!("https://{host}")).unwrap();
            let b = ServerKey::parse(&format!("HTTPS://{}:443/", host.to_uppercase())).unwrap();
            prop_assert_eq!(a, b);
        }
    }
}
