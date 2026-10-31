use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// An IPv4 or IPv6 network in CIDR notation; a bare address is a host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}
impl Cidr {
    pub fn parse(text: &str) -> Result<Self, BoxError> {
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text, None),
        };
        let addr: IpAddr = addr.parse()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .parse()
                .ok()
                .filter(|p| *p <= max)
                .ok_or("invalid CIDR prefix")?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        if self.addr.is_ipv4() != ip.is_ipv4() {
            return false;
        }
        let bits = |a: IpAddr| -> u128 {
            match a {
                IpAddr::V4(a) => u32::from(a) as u128,
                IpAddr::V6(a) => u128::from(a),
            }
        };
        let width = if ip.is_ipv4() { 32 } else { 128 };
        let shift = width - u32::from(self.prefix);
        shift >= 128 || bits(self.addr) >> shift == bits(ip) >> shift
    }
}
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub auth: AuthConfig,
    pub trusted_forwarders: Vec<String>,
    pub clone_urls: CloneUrls,
    pub default_email_domain: String,
    pub limits: Limits,
    pub cache: CacheConfig,
    pub session: SessionConfig,
}
impl Default for FileConfig {
    fn default() -> Self {
        Self {
            auth: AuthConfig::default(),
            trusted_forwarders: Vec::new(),
            clone_urls: CloneUrls::default(),
            default_email_domain: "users.kasu.invalid".into(),
            limits: Limits::default(),
            cache: CacheConfig::default(),
            session: SessionConfig::default(),
        }
    }
}
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub pat_login: bool,
    pub proxy: Option<ProxyConfig>,
    pub anonymous: AnonymousConfig,
    /// Login attempts allowed per client address and minute.
    pub login_attempts_per_minute: u32,
}
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            pat_login: true,
            proxy: None,
            anonymous: AnonymousConfig::default(),
            login_attempts_per_minute: 10,
        }
    }
}
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub trusted_peers: Vec<String>,
    pub user_header: String,
    #[serde(default)]
    pub email_header: Option<String>,
    #[serde(default)]
    pub name_header: Option<String>,
    #[serde(default)]
    pub accounts: ProxyAccounts,
}
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyAccounts {
    pub map: BTreeMap<String, String>,
    pub default_from_user: bool,
}
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnonymousConfig {
    pub enabled: bool,
    /// The server delegate's `anonymous_account`.
    pub account: Option<String>,
}
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloneUrls {
    pub http: Option<String>,
    pub ssh: Option<String>,
}
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub file_view_bytes: u64,
    pub highlight_bytes: u64,
    pub highlight_lines: usize,
    pub blame_lines: usize,
    pub diff_files: usize,
    pub diff_lines: usize,
    pub dir_entries: usize,
    pub upload_bytes: u64,
    pub upload_total_bytes: u64,
    pub highlight_concurrency: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            file_view_bytes: 1024 * 1024,
            highlight_bytes: 512 * 1024,
            highlight_lines: 20_000,
            blame_lines: 20_000,
            diff_files: 100,
            diff_lines: 2_000,
            dir_entries: 5_000,
            upload_bytes: 10 * 1024 * 1024,
            upload_total_bytes: 32 * 1024 * 1024,
            highlight_concurrency: 4,
        }
    }
}
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub bytes: usize,
}
impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            bytes: 256 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub idle_secs: u64,
    pub max_secs: u64,
}
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_secs: 2 * 3600,
            max_secs: 12 * 3600,
        }
    }
}

pub struct Config {
    pub listen: SocketAddr,
    pub server: String,
    /// None: the server runs without authentication (development).
    pub delegate_token: Option<String>,
    pub session_key: Vec<u8>,
    /// Scheme and authority, without a trailing slash.
    pub public_url: String,
    pub metrics_token: Option<String>,
    pub file: FileConfig,
    pub proxy_peers: Vec<Cidr>,
    pub forwarder_peers: Vec<Cidr>,
}
impl Config {
    pub fn secure(&self) -> bool {
        self.public_url.starts_with("https://")
    }
    /// A configuration for tests and development.
    pub fn local(server: &str) -> Self {
        Self {
            listen: "127.0.0.1:0".parse().expect("a socket address"),
            server: server.into(),
            delegate_token: None,
            session_key: vec![42; 32],
            public_url: "http://kasu.test".into(),
            metrics_token: None,
            file: FileConfig::default(),
            proxy_peers: Vec::new(),
            forwarder_peers: Vec::new(),
        }
    }
    pub fn validate(&mut self) -> Result<(), BoxError> {
        if self.session_key.len() < 32 {
            return Err("the session key needs at least 32 bytes".into());
        }
        let url = self.public_url.trim_end_matches('/').to_owned();
        let authority = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or("KASU_WEB_PUBLIC_URL must be an http(s) URL")?;
        if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
            return Err("KASU_WEB_PUBLIC_URL must be scheme://host[:port]".into());
        }
        self.public_url = url;
        let limits = &self.file.limits;
        if limits.file_view_bytes == 0
            || limits.upload_bytes == 0
            || limits.upload_total_bytes < limits.upload_bytes
            || limits.upload_total_bytes > 256 * 1024 * 1024
            || !(1..=64).contains(&limits.highlight_concurrency)
            || limits.diff_files == 0
            || limits.dir_entries == 0
        {
            return Err("invalid limits".into());
        }
        let session = &self.file.session;
        if session.idle_secs == 0 || session.max_secs < session.idle_secs {
            return Err("invalid session lifetimes".into());
        }
        self.forwarder_peers = self
            .file
            .trusted_forwarders
            .iter()
            .map(|c| Cidr::parse(c))
            .collect::<Result<_, _>>()?;
        self.proxy_peers.clear();
        if let Some(proxy) = &self.file.auth.proxy {
            if proxy.user_header.is_empty() {
                return Err("auth.proxy.user_header is required".into());
            }
            self.proxy_peers = proxy
                .trusted_peers
                .iter()
                .map(|c| Cidr::parse(c))
                .collect::<Result<_, _>>()?;
            if self.proxy_peers.is_empty() {
                return Err("auth.proxy.trusted_peers must not be empty".into());
            }
        }
        if self.file.auth.anonymous.enabled
            && self.delegate_token.is_some()
            && self.file.auth.anonymous.account.is_none()
        {
            return Err("auth.anonymous.account names the delegate's anonymous account".into());
        }
        Ok(())
    }
    pub fn from_env() -> Result<Self, BoxError> {
        let var = |name: &str| std::env::var_os(name);
        let text = |name: &str| -> Result<Option<String>, BoxError> {
            var(name)
                .map(|v| {
                    v.into_string()
                        .map_err(|_| format!("{name} must be UTF-8").into())
                })
                .transpose()
        };
        let secret = |path: PathBuf| -> Result<Vec<u8>, BoxError> {
            let mut bytes =
                std::fs::read(&path).map_err(|_| format!("cannot read {}", path.display()))?;
            while bytes.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                bytes.pop();
            }
            if bytes.is_empty() {
                return Err(format!("{} is empty", path.display()).into());
            }
            Ok(bytes)
        };
        let token = |path: PathBuf| -> Result<String, BoxError> {
            String::from_utf8(secret(path)?).map_err(|_| "tokens must be UTF-8".into())
        };
        let file = match var("KASU_WEB_CONFIG_FILE") {
            Some(path) => load_file(Path::new(&path))?,
            None => FileConfig::default(),
        };
        let mut config = Self {
            listen: text("KASU_WEB_LISTEN")?
                .unwrap_or_else(|| "127.0.0.1:50090".into())
                .parse()?,
            server: text("KASU_WEB_SERVER")?.ok_or("KASU_WEB_SERVER is required")?,
            delegate_token: var("KASU_WEB_DELEGATE_TOKEN_FILE")
                .map(|p| token(p.into()))
                .transpose()?,
            session_key: secret(
                var("KASU_WEB_SESSION_KEY_FILE")
                    .ok_or("KASU_WEB_SESSION_KEY_FILE is required")?
                    .into(),
            )?,
            public_url: text("KASU_WEB_PUBLIC_URL")?.ok_or("KASU_WEB_PUBLIC_URL is required")?,
            metrics_token: var("KASU_WEB_METRICS_TOKEN_FILE")
                .map(|p| token(p.into()))
                .transpose()?,
            file,
            proxy_peers: Vec::new(),
            forwarder_peers: Vec::new(),
        };
        config.validate()?;
        if config.delegate_token.is_none() {
            tracing::warn!(
                "KASU_WEB_DELEGATE_TOKEN_FILE is unset: every visitor shares the server's unauthenticated access (development only)"
            );
        }
        if !config.secure() {
            tracing::warn!(
                "KASU_WEB_PUBLIC_URL is http: cookies are not Secure (development only)"
            );
        }
        Ok(config)
    }
}
fn load_file(path: &Path) -> Result<FileConfig, BoxError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err("KASU_WEB_CONFIG_FILE exceeds 1 MiB".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_is_strict_and_validated() {
        let parsed: FileConfig = serde_json::from_str(
            r#"{"auth":{"pat_login":true,"proxy":{"trusted_peers":["10.0.0.0/8"],"user_header":"X-Forwarded-User","accounts":{"map":{"a@example.com":"alice"},"default_from_user":false}},"anonymous":{"enabled":true,"account":"public"}},"clone_urls":{"http":"https://kasu.example"},"limits":{"file_view_bytes":10,"upload_bytes":5,"upload_total_bytes":6}}"#,
        )
        .unwrap();
        let mut config = Config::local("http://127.0.0.1:1");
        config.file = parsed;
        config.validate().unwrap();
        assert!(config.proxy_peers[0].contains("10.1.2.3".parse().unwrap()));
        assert!(serde_json::from_str::<FileConfig>(r#"{"typo":1}"#).is_err());
        config.public_url = "https://kasu.example/path".into();
        assert!(config.validate().is_err());
        config.public_url = "https://kasu.example/".into();
        config.validate().unwrap();
        assert_eq!(config.public_url, "https://kasu.example");
        config.session_key = vec![1; 8];
        assert!(config.validate().is_err());
    }
}
