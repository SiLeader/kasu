pub mod csrf;
pub mod session;

use crate::{App, config::canonical, upstream::Caller};
use axum::http::HeaderMap;
pub use session::Session;
use std::net::IpAddr;

/// How the browser is identified.
#[derive(Clone, Debug)]
pub enum Identity {
    Anonymous,
    Session(Session),
    Proxy {
        user: String,
        account: String,
        email: Option<String>,
        name: Option<String>,
    },
}
/// The resolved visitor of one request.
#[derive(Clone, Debug)]
pub struct Viewer {
    pub identity: Identity,
    pub caller: Caller,
    /// What CSRF tokens bind to.
    pub csrf_subject: String,
    /// A fresh visitor cookie to set (anonymous visitors only).
    pub new_visitor: Option<String>,
}
impl Viewer {
    pub fn account(&self) -> Option<&str> {
        match &self.identity {
            Identity::Anonymous => None,
            Identity::Session(s) => Some(&s.account),
            Identity::Proxy { account, .. } => Some(account),
        }
    }
    pub fn signed_in(&self) -> bool {
        !matches!(self.identity, Identity::Anonymous)
    }
}
pub enum Refusal {
    /// Sign in first.
    LoginRequired,
    Forbidden,
}
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
}
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
impl App {
    pub fn session_cookie(&self) -> &'static str {
        if self.config.secure() {
            "__Host-kasu_session"
        } else {
            "kasu_session"
        }
    }
    pub fn visitor_cookie(&self) -> &'static str {
        if self.config.secure() {
            "__Host-kasu_visitor"
        } else {
            "kasu_visitor"
        }
    }
    pub fn cookie_attributes(&self) -> &'static str {
        if self.config.secure() {
            "; Path=/; HttpOnly; Secure; SameSite=Lax"
        } else {
            "; Path=/; HttpOnly; SameSite=Lax"
        }
    }
    fn trusted_proxy(&self, peer: Option<IpAddr>) -> bool {
        peer.is_some_and(|ip| self.config.proxy_peers.iter().any(|c| c.contains(ip)))
    }
    /// The browser's address: the peer, or the proxy's forwarded address.
    pub fn client_addr(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Option<IpAddr> {
        if (self.trusted_proxy(peer)
            || peer.is_some_and(|ip| self.config.forwarder_peers.iter().any(|c| c.contains(ip))))
            && let Some(forwarded) = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse().ok())
        {
            return Some(canonical(forwarded));
        }
        peer.map(canonical)
    }
    /// Identify the visitor: trusted proxy headers, then the session, then
    /// anonymous access.
    pub fn viewer(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Result<Viewer, Refusal> {
        let client = self.client_addr(headers, peer);
        let auth = &self.config.file.auth;
        if let Some(proxy) = &auth.proxy {
            let header = |name: &Option<String>| {
                name.as_ref()
                    .and_then(|n| headers.get(n.as_str()))
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
                    .filter(|v| !v.is_empty() && v.len() <= 256)
            };
            let user = header(&Some(proxy.user_header.clone()));
            if user.is_some() && !self.trusted_proxy(peer) {
                tracing::warn!(
                    ?peer,
                    "ignored a proxy identity header from an untrusted peer"
                );
            } else if let Some(user) = user {
                let account = proxy
                    .accounts
                    .map
                    .get(&user)
                    .cloned()
                    .or_else(|| proxy.accounts.default_from_user.then(|| user.clone()));
                match account {
                    Some(account) => {
                        return Ok(Viewer {
                            caller: Caller {
                                act_as: Some(account.clone()),
                                binding: None,
                                user: Some(user.clone()),
                                client,
                            },
                            csrf_subject: format!("proxy:{user}"),
                            identity: Identity::Proxy {
                                email: header(&proxy.email_header),
                                name: header(&proxy.name_header),
                                user,
                                account,
                            },
                            new_visitor: None,
                        });
                    }
                    None if !auth.anonymous.enabled => return Err(Refusal::Forbidden),
                    None => {}
                }
            }
        }
        let session = &self.config.file.session;
        if let Some(value) = cookie(headers, self.session_cookie())
            && let Some(session) = Session::decode(
                &self.config.session_key,
                value,
                now(),
                session.idle_secs as i64,
                session.max_secs as i64,
            )
        {
            return Ok(Viewer {
                caller: Caller {
                    act_as: Some(session.account.clone()),
                    binding: Some(session.binding.clone()),
                    user: Some(session.account.clone()),
                    client,
                },
                csrf_subject: format!("session:{}", hex::encode(session.id)),
                identity: Identity::Session(session),
                new_visitor: None,
            });
        }
        if !auth.anonymous.enabled && self.upstream.delegated() {
            return Err(Refusal::LoginRequired);
        }
        let (visitor, new_visitor) = match cookie(headers, self.visitor_cookie())
            .filter(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            Some(v) => (v.to_owned(), None),
            None => {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).expect("the operating system provides randomness");
                let value = hex::encode(bytes);
                (value.clone(), Some(value))
            }
        };
        Ok(Viewer {
            identity: Identity::Anonymous,
            caller: Caller {
                act_as: auth.anonymous.account.clone(),
                binding: None,
                user: None,
                client,
            },
            csrf_subject: format!("visitor:{visitor}"),
            new_visitor,
        })
    }
    /// A viewer that may only reach the login page.
    pub fn login_viewer(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Viewer {
        let mut viewer = Viewer {
            identity: Identity::Anonymous,
            caller: Caller {
                client: self.client_addr(headers, peer),
                ..Default::default()
            },
            csrf_subject: String::new(),
            new_visitor: None,
        };
        match cookie(headers, self.visitor_cookie())
            .filter(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            Some(v) => viewer.csrf_subject = format!("visitor:{v}"),
            None => {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).expect("the operating system provides randomness");
                let value = hex::encode(bytes);
                viewer.csrf_subject = format!("visitor:{value}");
                viewer.new_visitor = Some(value);
            }
        }
        viewer
    }
}
