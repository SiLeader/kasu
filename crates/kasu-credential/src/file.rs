//! `file` helper: plaintext JSON, mode 0600, the last resort.

use crate::{
    Backend, Credential, Error, Result, ServerKey,
    config::{config_dir, create_private_dir, lock, write_atomic},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
struct Data {
    version: u32,
    credentials: BTreeMap<String, Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    secret: String,
}

impl Default for Data {
    fn default() -> Self {
        Self {
            version: 1,
            credentials: BTreeMap::new(),
        }
    }
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `$KASU_CREDENTIALS_FILE`, else `credentials.json` in the config directory.
    pub fn default_path() -> Result<PathBuf> {
        match std::env::var_os("KASU_CREDENTIALS_FILE").filter(|v| !v.is_empty()) {
            Some(path) => Ok(path.into()),
            None => Ok(config_dir()?.join("credentials.json")),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn read(&self) -> Result<Data> {
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Data::default()),
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata()?;
        if meta.uid() != rustix::process::getuid().as_raw() || meta.mode() & 0o077 != 0 {
            eprintln!(
                "kasu: warning: {} is accessible to other users; run 'chmod 600'",
                self.path.display()
            );
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.read_to_end(&mut bytes)?;
        let data: Data = serde_json::from_slice(&bytes).map_err(|_| {
            Error::Config(format!(
                "{} is not a valid credentials file",
                self.path.display()
            ))
        })?;
        if data.version != 1 {
            return Err(Error::Config(format!(
                "{} has an unsupported version",
                self.path.display()
            )));
        }
        Ok(data)
    }

    fn write(&self, data: &Data) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir)?;
        }
        let mut json = Zeroizing::new(
            serde_json::to_vec_pretty(data)
                .map_err(|_| Error::Config("cannot serialise credentials".into()))?,
        );
        json.push(b'\n');
        write_atomic(&self.path, &json, 0o600)?;
        Ok(())
    }

    fn lock(&self) -> Result<crate::config::Lock> {
        let mut name = self.path.clone().into_os_string();
        name.push(".lock");
        lock(&PathBuf::from(name))
    }
}

impl Backend for FileStore {
    fn get(&self, server: &ServerKey) -> Result<Option<Credential>> {
        Ok(self
            .read()?
            .credentials
            .remove(server.as_str())
            .map(|e| Credential {
                server: server.clone(),
                account: e.account,
                secret: Zeroizing::new(e.secret),
            }))
    }

    fn store(&self, credential: &Credential) -> Result<()> {
        let _lock = self.lock()?;
        let mut data = self.read()?;
        data.credentials.insert(
            credential.server.to_string(),
            Entry {
                account: credential.account.clone(),
                secret: credential.secret.to_string(),
            },
        );
        self.write(&data)
    }

    fn erase(&self, server: &ServerKey) -> Result<bool> {
        let _lock = self.lock()?;
        let mut data = self.read()?;
        let removed = data.credentials.remove(server.as_str()).is_some();
        if removed {
            self.write(&data)?;
        }
        Ok(removed)
    }

    fn list(&self) -> Result<Vec<(ServerKey, Option<String>)>> {
        self.read()?
            .credentials
            .into_iter()
            .map(|(server, e)| Ok((ServerKey::parse(&server)?, e.account)))
            .collect()
    }
}
