use crate::*;
use std::{path::Path, sync::Arc};

pub trait DatabaseStore:
    MetaStore + GitRefStore + WebhookStore + RepositoryStore + ReviewStore
{
}
impl<T: MetaStore + GitRefStore + WebhookStore + RepositoryStore + ReviewStore> DatabaseStore
    for T
{
}

pub struct Storage {
    pub database: Arc<dyn DatabaseStore>,
    pub blobs: Arc<dyn BlobStore>,
}
impl Storage {
    /// Open the configured shared stores, or SQLite/FS in `local` by default.
    /// External store configuration errors never fall back to local storage.
    pub fn from_env(local: &Path) -> Result<Self> {
        let database: Arc<dyn DatabaseStore> =
            if let Some(path) = std::env::var_os("KASU_POSTGRES_DSN_FILE") {
                postgres(&std::path::PathBuf::from(path))?
            } else {
                std::fs::create_dir_all(local).map_err(storage_error)?;
                Arc::new(SqliteStore::open(local.join("meta.db"))?)
            };
        let blobs: Arc<dyn BlobStore> = if let Some(bucket) = env("KASU_S3_BUCKET")? {
            s3(bucket)?
        } else {
            Arc::new(FsBlobStore::open(local.join("objects"))?)
        };
        Ok(Self { database, blobs })
    }
}
fn env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if value.is_empty() => Err(invalid(format!("{name} must not be empty"))),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(invalid(format!("{name} must be UTF-8"))),
    }
}
#[cfg(any(feature = "s3", feature = "postgres"))]
fn flag(name: &str) -> Result<bool> {
    match env(name)?.as_deref() {
        None | Some("false" | "0") => Ok(false),
        Some("true" | "1") => Ok(true),
        _ => Err(invalid(format!("{name} must be true/false or 1/0"))),
    }
}
#[cfg(feature = "postgres")]
fn postgres(path: &Path) -> Result<Arc<dyn DatabaseStore>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(storage_error)?
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(storage_error)?;
    if bytes.is_empty() || bytes.len() > 65536 {
        return Err(invalid("invalid PostgreSQL DSN file size"));
    }
    let dsn = String::from_utf8(bytes).map_err(|_| invalid("PostgreSQL DSN must be UTF-8"))?;
    let mut options = PostgresOptions::new(dsn.trim_end_matches(['\r', '\n']).to_owned());
    options.ca_file = std::env::var_os("KASU_POSTGRES_CA_FILE").map(Into::into);
    options.allow_plaintext = flag("KASU_POSTGRES_ALLOW_PLAINTEXT")?;
    Ok(Arc::new(PostgresStore::connect(options)?))
}
#[cfg(not(feature = "postgres"))]
fn postgres(_: &Path) -> Result<Arc<dyn DatabaseStore>> {
    Err(invalid(
        "PostgreSQL support requires the production or postgres feature",
    ))
}
#[cfg(feature = "s3")]
fn s3(bucket: String) -> Result<Arc<dyn BlobStore>> {
    let mut options = S3Options::new(bucket);
    if let Some(prefix) = env("KASU_S3_PREFIX")? {
        options.prefix = prefix;
    }
    options.endpoint = env("KASU_S3_ENDPOINT")?;
    options.region = env("KASU_S3_REGION")?;
    options.path_style = flag("KASU_S3_PATH_STYLE")?;
    options.allow_plaintext = flag("KASU_S3_ALLOW_PLAINTEXT")?;
    Ok(Arc::new(S3BlobStore::connect(options)?))
}
#[cfg(not(feature = "s3"))]
fn s3(_: String) -> Result<Arc<dyn BlobStore>> {
    Err(invalid("S3 support requires the production or s3 feature"))
}
