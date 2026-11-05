//! Pluggable credential storage for the `kasu` CLI (see
//! `docs/plan/credential-helper`).
//!
//! A [`Helper`] stores one personal access token per [`ServerKey`]. Built-in
//! helpers talk to the OS secret store (`osxkeychain`, `secretservice`),
//! `pass`, or a plaintext `file`; any other name is run as the external
//! executable `kasu-credential-<name>` that speaks the JSON [`protocol`].

mod config;
mod external;
mod file;
mod key;
mod pass;
pub mod protocol;
mod secretservice;

#[cfg(target_os = "macos")]
mod osxkeychain;

pub use config::{Config, HelperConfig, ServerConfig, config_dir};
pub use file::FileStore;
pub use key::ServerKey;

use std::{fmt, sync::Arc};
use zeroize::Zeroizing;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The helper itself cannot be used (no D-Bus session, `pass` not
    /// initialised, wrong operating system, ...).
    #[error("{0}")]
    NotAvailable(String),
    #[error("credential helper does not support this operation")]
    Unsupported,
    #[error("credential helper '{name}' failed: {message}")]
    Helper { name: String, message: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Config(String),
    #[error("invalid server: {0}")]
    InvalidKey(String),
}

/// Helpers implemented inside this crate. Anything else is an external helper.
pub const BUILTIN_HELPERS: &[&str] = &["osxkeychain", "secretservice", "pass", "file"];

/// A stored personal access token. `Debug` never prints the secret.
#[derive(Clone)]
pub struct Credential {
    pub server: ServerKey,
    /// Account name reported by `Identity.WhoAmI`; `None` if unverified.
    pub account: Option<String>,
    pub secret: Zeroizing<String>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("server", &self.server)
            .field("account", &self.account)
            .field("secret", &"***")
            .finish()
    }
}

impl Credential {
    /// Rejects values no helper could round-trip or that the server cannot receive.
    pub fn validate(&self) -> Result<()> {
        validate_secret(&self.secret)?;
        if let Some(account) = &self.account
            && account.chars().any(char::is_control)
        {
            return Err(Error::Config(
                "account name contains control characters".into(),
            ));
        }
        Ok(())
    }
}

/// A token must be non-empty, printable ASCII-compatible text without
/// surrounding whitespace, at most 4096 bytes (the limit of `Authorization`).
pub fn validate_secret(secret: &str) -> Result<()> {
    let bad = |m: &str| Err(Error::Config(m.into()));
    if secret.is_empty() {
        return bad("the token is empty");
    }
    if secret.len() > 4096 {
        return bad("the token is too long");
    }
    if secret.chars().any(char::is_control) {
        return bad("the token contains control characters");
    }
    if secret.trim() != secret {
        return bad("the token has leading or trailing whitespace");
    }
    Ok(())
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = name.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "invalid credential helper name {name:?}"
        )))
    }
}

/// Storage operations (the synchronous core is [`Backend`]).
#[allow(async_fn_in_trait)]
pub trait Store {
    fn name(&self) -> &str;
    async fn get(&self, server: &ServerKey) -> Result<Option<Credential>>;
    async fn store(&self, credential: &Credential) -> Result<()>;
    /// `Ok(false)` if nothing was stored for `server`.
    async fn erase(&self, server: &ServerKey) -> Result<bool>;
    /// Entries without secrets; [`Error::Unsupported`] if the helper cannot list.
    async fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>>;
}

pub(crate) trait Backend: Send + Sync {
    fn get(&self, server: &ServerKey) -> Result<Option<Credential>>;
    fn store(&self, credential: &Credential) -> Result<()>;
    fn erase(&self, server: &ServerKey) -> Result<bool>;
    fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>>;
}

/// A named, opened credential helper.
#[derive(Clone)]
pub struct Helper {
    name: String,
    backend: Arc<dyn Backend>,
}

impl fmt::Debug for Helper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Helper").field(&self.name).finish()
    }
}

impl Helper {
    /// Opens the built-in helper `name`, or an external `kasu-credential-<name>`.
    pub fn open(name: &str, config: &Config) -> Result<Self> {
        validate_name(name)?;
        let backend: Arc<dyn Backend> = match name {
            "file" => Arc::new(FileStore::new(FileStore::default_path()?)),
            "pass" => Arc::new(pass::Pass::new()?),
            #[cfg(target_os = "linux")]
            "secretservice" => Arc::new(secretservice::SecretService::new()?),
            #[cfg(not(target_os = "linux"))]
            "secretservice" => {
                return Err(Error::NotAvailable(
                    "secretservice is only available on Linux".into(),
                ));
            }
            #[cfg(target_os = "macos")]
            "osxkeychain" => Arc::new(osxkeychain::OsxKeychain),
            #[cfg(not(target_os = "macos"))]
            "osxkeychain" => {
                return Err(Error::NotAvailable(
                    "osxkeychain is only available on macOS".into(),
                ));
            }
            _ => Arc::new(external::External::new(name, config)),
        };
        Ok(Self {
            name: name.to_owned(),
            backend,
        })
    }

    /// Wraps a store implementation (used by tests and embedders).
    #[doc(hidden)]
    pub fn from_file_store(store: FileStore) -> Self {
        Self {
            name: "file".into(),
            backend: Arc::new(store),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn get_blocking(&self, server: &ServerKey) -> Result<Option<Credential>> {
        self.backend.get(server)
    }
    pub fn store_blocking(&self, credential: &Credential) -> Result<()> {
        credential.validate()?;
        self.backend.store(credential)
    }
    pub fn erase_blocking(&self, server: &ServerKey) -> Result<bool> {
        self.backend.erase(server)
    }
    pub fn list_blocking(&self) -> Result<Vec<(ServerKey, Option<String>)>> {
        self.backend.list()
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Error::Config("credential helper task failed".into()))?
}

impl Store for Helper {
    fn name(&self) -> &str {
        &self.name
    }
    async fn get(&self, server: &ServerKey) -> Result<Option<Credential>> {
        let (this, server) = (self.clone(), server.clone());
        blocking(move || this.get_blocking(&server)).await
    }
    async fn store(&self, credential: &Credential) -> Result<()> {
        let (this, credential) = (self.clone(), credential.clone());
        blocking(move || this.store_blocking(&credential)).await
    }
    async fn erase(&self, server: &ServerKey) -> Result<bool> {
        let (this, server) = (self.clone(), server.clone());
        blocking(move || this.erase_blocking(&server)).await
    }
    async fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>> {
        let this = self.clone();
        blocking(move || this.list_blocking()).await
    }
}

/// The platform's default helper when nothing is configured.
pub fn platform_default() -> &'static str {
    if cfg!(target_os = "macos") {
        "osxkeychain"
    } else {
        "secretservice"
    }
}

/// Picks the helper name: `explicit` > `KASU_CREDENTIAL_HELPER` > the helper
/// recorded for `server` > the configured default > the platform default.
/// The result can be `"none"` (from the environment) meaning "no helper".
pub fn select_helper(
    explicit: Option<&str>,
    server: Option<&ServerKey>,
    config: &Config,
) -> String {
    if let Some(name) = explicit {
        return name.to_owned();
    }
    if let Ok(name) = std::env::var("KASU_CREDENTIAL_HELPER")
        && !name.is_empty()
    {
        return name;
    }
    if let Some(name) = server
        .and_then(|s| config.servers.get(s.as_str()))
        .and_then(|s| s.credential_helper.clone())
    {
        return name;
    }
    config
        .credential_helper
        .clone()
        .unwrap_or_else(|| platform_default().to_owned())
}
