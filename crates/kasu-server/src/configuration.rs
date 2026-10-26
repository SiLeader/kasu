use crate::Backend;
use kasu_store::{DatabaseStore, Storage};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct ConfiguredBackend {
    pub backend: Backend,
    pub database: Arc<dyn DatabaseStore>,
}
pub fn load_backend(
    root: &Path,
    repository: Vec<u8>,
) -> Result<ConfiguredBackend, Box<dyn std::error::Error + Send + Sync>> {
    let Storage { database, blobs } = Storage::from_env(root)?;
    let token = match (
        std::env::var_os("KASU_TOKEN_FILE"),
        std::env::var("KASU_TOKEN"),
    ) {
        (Some(_), Ok(_)) => return Err("use only one of KASU_TOKEN and KASU_TOKEN_FILE".into()),
        (Some(path), Err(std::env::VarError::NotPresent)) => {
            Some(kasu_object::read_token(&PathBuf::from(path))?)
        }
        (None, Ok(value)) if !value.is_empty() => Some(value),
        (None, Ok(_)) => return Err("KASU_TOKEN must not be empty".into()),
        (None, Err(std::env::VarError::NotPresent)) => None,
        (_, Err(error)) => return Err(error.into()),
    };
    let mut backend = Backend::new(repository, database.clone(), blobs, database.clone(), token);
    if let Some(path) = std::env::var_os("KASU_WRITE_POLICY_FILE") {
        backend = backend.with_write_policy(serde_json::from_slice(&std::fs::read(path)?)?);
    }
    if let Some(path) = std::env::var_os("KASU_OBJECT_TOKEN_FILE") {
        backend = backend.with_object_token(kasu_object::read_token(&PathBuf::from(path))?)?;
    }
    if let Some(path) = std::env::var_os("KASU_PROJECTION_ACCOUNTS_FILE") {
        backend = backend
            .with_projection_accounts(crate::load_projection_accounts(&PathBuf::from(path))?)?;
    }
    if let Some(path) = std::env::var_os("KASU_ACCOUNTS_FILE") {
        backend = backend
            .with_repository_accounts(crate::load_repository_accounts(&PathBuf::from(path))?)?;
    }
    if let Some(path) = std::env::var_os("KASU_SERVER_LIMITS_FILE") {
        backend = backend.with_limits(crate::ServerLimits::load(&PathBuf::from(path))?)?;
    }
    Ok(ConfiguredBackend { backend, database })
}
