use crate::{Backend, auth::Credential};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::net::IpAddr;
use tonic::Status;

/// Metadata a delegate attaches to act as an account.
pub const ACT_AS: &str = "kasu-act-as";
pub const ACT_BINDING: &str = "kasu-act-binding";
pub const ACT_USER: &str = "kasu-act-user";
pub const CLIENT_ADDR: &str = "kasu-client-addr";
/// The authorization value the admission layer substitutes for a verified
/// delegated request. It carries a per-process nonce, so it cannot be sent
/// from outside; external values with this scheme are rejected.
const INTERNAL: &str = "KasuDelegated ";

/// An IPv4 or IPv6 network in CIDR notation; a bare address is a host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}
impl Cidr {
    pub fn parse(text: &str) -> Result<Self, Status> {
        let invalid = || Status::invalid_argument("invalid CIDR");
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| invalid())?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p.parse().ok().filter(|p| *p <= max).ok_or_else(invalid)?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        let bits = |a: IpAddr| -> u128 {
            match a {
                IpAddr::V4(a) => u32::from(a) as u128,
                IpAddr::V6(a) => u128::from(a),
            }
        };
        if self.addr.is_ipv4() != ip.is_ipv4() {
            return false;
        }
        let width = if ip.is_ipv4() { 32 } else { 128 };
        let shift = width - u32::from(self.prefix);
        shift >= 128 || bits(self.addr) >> shift == bits(ip) >> shift
    }
}

/// A principal that cannot read anything itself but may act as accounts,
/// such as the Web UI. Deliberately not Debug: it holds a credential.
pub struct Delegate {
    pub name: String,
    pub token: String,
    /// Account name globs this delegate may act as.
    pub act_as: Vec<String>,
    /// Whether delegated requests may administer repositories.
    pub allow_admin: bool,
    /// Whether requests without a PAT binding (proxy authentication) are allowed.
    pub allow_unbound: bool,
    /// The account anonymous visitors act as; always read-only.
    pub anonymous_account: Option<String>,
    /// Source networks this delegate may connect from; empty allows any.
    pub trusted_peers: Vec<Cidr>,
}
/// A verified delegated request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Delegation {
    pub account: usize,
    pub delegate: u16,
    pub anonymous: bool,
}
/// The outcome of checking delegation metadata in the admission layer.
pub(crate) struct Verified {
    /// The replacement authorization value.
    pub authorization: String,
    pub delegation: Delegation,
    /// The browser's address, used for per-client admission.
    pub client: Option<IpAddr>,
}

impl Backend {
    pub fn with_delegates(mut self, delegates: Vec<Delegate>) -> Result<Self, Status> {
        if delegates.len() > 16 {
            return Err(Status::invalid_argument("at most 16 delegates"));
        }
        let mut names = std::collections::BTreeSet::new();
        for (i, d) in delegates.iter().enumerate() {
            kasu_object::bearer(&d.token)?;
            let overlapping = self.token.as_deref() == Some(&d.token)
                || self.object_token.as_deref() == Some(&d.token)
                || self.repository_accounts.iter().any(|a| a.token == d.token)
                || self.projection_accounts.iter().any(|a| a.token == d.token)
                || delegates[..i].iter().any(|o| o.token == d.token);
            if d.name.is_empty()
                || d.name.len() > 128
                || !d.name.bytes().all(|b| b.is_ascii_graphic())
                || !names.insert(&d.name)
                || overlapping
                || d.act_as.len() > 128
                || d.act_as.iter().any(|g| g.is_empty() || g.len() > 128)
                || d.trusted_peers.len() > 128
                || d.anonymous_account
                    .as_ref()
                    .is_some_and(|name| !self.repository_accounts.iter().any(|a| &a.name == name))
            {
                return Err(Status::invalid_argument(
                    "invalid or overlapping delegate configuration",
                ));
            }
            if d.trusted_peers.is_empty() {
                tracing::warn!(delegate=%d.name, "delegate accepts connections from any address; set trusted_peers");
            }
        }
        self.delegates = std::sync::Arc::new(delegates);
        Ok(self)
    }
    /// Sessions bind to a keyed hash of the PAT, so replacing it ends them.
    pub fn with_binding_key(mut self, key: Vec<u8>) -> Result<Self, Status> {
        if key.len() < 32 {
            return Err(Status::invalid_argument(
                "binding key must have at least 32 bytes",
            ));
        }
        self.binding_key = std::sync::Arc::new(key);
        Ok(self)
    }
    pub(crate) fn binding(&self, account: usize) -> Vec<u8> {
        let account = &self.repository_accounts[account];
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.binding_key)
            .expect("HMAC accepts keys of all sizes");
        mac.update(b"kasu.binding.v1\0");
        mac.update(account.name.as_bytes());
        mac.update(b"\0");
        mac.update(account.token.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
    /// Check the delegation metadata of a request from `peer`. None when
    /// the request is not delegated.
    pub(crate) fn verify_delegation(
        &self,
        header: &dyn Fn(&str) -> Option<String>,
        peer: Option<IpAddr>,
    ) -> Result<Option<Verified>, Status> {
        let authorization = header("authorization");
        if authorization
            .as_deref()
            .is_some_and(|a| a.starts_with(INTERNAL))
        {
            return Err(Status::unauthenticated("invalid credentials"));
        }
        let Some(act_as) = header(ACT_AS) else {
            return Ok(None);
        };
        let token = authorization
            .as_deref()
            .and_then(|a| a.strip_prefix("Bearer "))
            .ok_or_else(|| Status::unauthenticated("delegation requires a delegate token"))?;
        let (index, delegate) = self
            .delegates
            .iter()
            .enumerate()
            .find(|(_, d)| kasu_object::token_eq(token.as_bytes(), d.token.as_bytes()))
            .ok_or_else(|| Status::unauthenticated("delegation requires a delegate token"))?;
        if !delegate.trusted_peers.is_empty()
            && !peer.is_some_and(|ip| delegate.trusted_peers.iter().any(|c| c.contains(ip)))
        {
            tracing::warn!(delegate=%delegate.name, ?peer, "delegated request from an untrusted peer");
            return Err(Status::unauthenticated("delegate peer is not trusted"));
        }
        let anonymous = delegate.anonymous_account.as_deref() == Some(act_as.as_str());
        let account = self
            .repository_accounts
            .iter()
            .position(|a| a.name == act_as)
            .ok_or_else(|| Status::unauthenticated("unknown delegated account"))?;
        if !anonymous {
            if !delegate
                .act_as
                .iter()
                .any(|g| crate::write::glob(g.as_bytes(), act_as.as_bytes()))
            {
                return Err(Status::unauthenticated(
                    "account is outside the delegate scope",
                ));
            }
            match header(ACT_BINDING) {
                Some(binding) => {
                    let binding = hex::decode(binding)
                        .map_err(|_| Status::unauthenticated("invalid account binding"))?;
                    if !kasu_object::token_eq(&binding, &self.binding(account)) {
                        return Err(Status::unauthenticated("account credentials changed"));
                    }
                }
                None if delegate.allow_unbound => {}
                None => return Err(Status::unauthenticated("account binding required")),
            }
        }
        let user = header(ACT_USER).unwrap_or_default();
        if user.len() > 256 {
            return Err(Status::invalid_argument("kasu-act-user exceeds 256 bytes"));
        }
        let client = header(CLIENT_ADDR).and_then(|a| a.parse().ok());
        Ok(Some(Verified {
            authorization: format!(
                "{INTERNAL}{} {index} {account} {} {}",
                hex::encode(*self.delegation_nonce),
                u8::from(anonymous),
                hex::encode(user)
            ),
            delegation: Delegation {
                account,
                delegate: index as u16,
                anonymous,
            },
            client,
        }))
    }
    /// The credential of an internal delegated authorization value.
    pub(crate) fn delegated(&self, authorization: &str) -> Option<Result<Credential, Status>> {
        let rest = authorization.strip_prefix(INTERNAL)?;
        let invalid = || Status::unauthenticated("invalid credentials");
        Some((|| {
            let mut parts = rest.split(' ');
            let nonce = hex::decode(parts.next().ok_or_else(invalid)?).map_err(|_| invalid())?;
            if !kasu_object::token_eq(&nonce, self.delegation_nonce.as_slice()) {
                return Err(invalid());
            }
            let mut number = || -> Result<usize, Status> {
                parts
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(invalid)
            };
            let delegate = number()?;
            let account = number()?;
            let anonymous = number()? == 1;
            if delegate >= self.delegates.len() || account >= self.repository_accounts.len() {
                return Err(invalid());
            }
            Ok(Credential::Delegated(Delegation {
                account,
                delegate: delegate as u16,
                anonymous,
            }))
        })())
    }
    /// The audit annotation of a delegated request: delegate name and user.
    pub(crate) fn delegation_via(&self, authorization: Option<&str>) -> Option<Vec<u8>> {
        let rest = authorization?.strip_prefix(INTERNAL)?;
        let parts: Vec<&str> = rest.split(' ').collect();
        let delegate = self.delegates.get(parts.get(1)?.parse::<usize>().ok()?)?;
        let mut via = delegate.name.as_bytes().to_vec();
        if let Some(user) = parts.get(4).and_then(|u| hex::decode(u).ok())
            && !user.is_empty()
        {
            via.push(b':');
            via.extend(user);
        }
        Some(via)
    }
    pub(crate) fn delegate_config(&self, index: u16) -> &Delegate {
        &self.delegates[usize::from(index)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cidr_matches_networks_hosts_and_mapped_addresses() {
        let net = Cidr::parse("10.0.20.0/24").unwrap();
        assert!(net.contains("10.0.20.7".parse().unwrap()));
        assert!(net.contains("::ffff:10.0.20.7".parse().unwrap()));
        assert!(!net.contains("10.0.21.7".parse().unwrap()));
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains("1.2.3.4".parse().unwrap())
        );
        assert!(Cidr::parse("::1").unwrap().contains("::1".parse().unwrap()));
        assert!(
            !Cidr::parse("::1")
                .unwrap()
                .contains("127.0.0.1".parse().unwrap())
        );
        for bad in ["10.0.0.0/33", "x", "::/129", "1.2.3.4/"] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
    }
}
