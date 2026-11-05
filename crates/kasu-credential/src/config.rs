//! `config.json`: which helper holds the credentials of which server. Never
//! contains secrets.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{DirBuilder, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_helper: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HelperConfig {
    /// Absolute path of an external helper executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_helper: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, ServerConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub helpers: BTreeMap<String, HelperConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_timeout_secs: Option<u64>,
    /// Set when the file has a version this build does not understand.
    #[serde(skip)]
    read_only: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION,
            credential_helper: None,
            servers: BTreeMap::new(),
            helpers: BTreeMap::new(),
            helper_timeout_secs: None,
            read_only: false,
        }
    }
}

/// `$KASU_CONFIG_DIR`, else `$XDG_CONFIG_HOME/kasu`, else `~/.config/kasu`.
pub fn config_dir() -> Result<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(dir) = var("KASU_CONFIG_DIR") {
        return Ok(dir.into());
    }
    if let Some(dir) = var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("kasu"));
    }
    var("HOME")
        .map(|home| PathBuf::from(home).join(".config/kasu"))
        .ok_or_else(|| Error::Config("cannot locate the configuration directory".into()))
}

pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Holds an exclusive advisory lock until dropped.
pub(crate) struct Lock(#[allow(dead_code)] File);

pub(crate) fn lock(path: &Path) -> Result<Lock> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .map_err(std::io::Error::from)?;
    Ok(Lock(file))
}

/// Atomically replaces `path` with `contents` (mode `mode`), via a temporary
/// file in the same directory.
pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("kasu"),
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.json"))
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        Self::parse(&bytes).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
    }

    pub fn parse(bytes: &[u8]) -> std::result::Result<Self, String> {
        let mut config: Config =
            serde_json::from_slice(bytes).map_err(|_| "invalid config.json".to_string())?;
        if config.version != VERSION {
            config.read_only = true;
        }
        Ok(config)
    }

    /// Applies `change` to the on-disk configuration under an exclusive lock.
    pub fn update(change: impl FnOnce(&mut Config)) -> Result<()> {
        let path = Self::path()?;
        let _lock = lock(&path.with_extension("json.lock"))?;
        let mut config = Self::load_from(&path)?;
        if config.read_only {
            return Err(Error::Config(format!(
                "{} has an unsupported version; refusing to modify it",
                path.display()
            )));
        }
        change(&mut config);
        let mut json = serde_json::to_vec_pretty(&config)
            .map_err(|_| Error::Config("cannot serialise config.json".into()))?;
        json.push(b'\n');
        write_atomic(&path, &json, 0o644)?;
        Ok(())
    }

    pub fn helper_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.helper_timeout_secs.unwrap_or(60).clamp(1, 3600))
    }
}
