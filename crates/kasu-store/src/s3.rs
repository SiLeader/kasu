use crate::*;
use futures_util::StreamExt;
use object_store::{
    ObjectStore, ObjectStoreExt, PutMode,
    aws::{AmazonS3Builder, AmazonS3ConfigKey, S3ConditionalPut},
    path::Path,
};
use std::{sync::Arc, time::Duration};

pub struct S3Options {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub allow_plaintext: bool,
    pub path_style: bool,
    pub timeout: Duration,
}
impl S3Options {
    pub fn new(bucket: String) -> Self {
        Self {
            bucket,
            prefix: "kasu/objects/v1".into(),
            endpoint: None,
            region: None,
            allow_plaintext: false,
            path_style: false,
            timeout: Duration::from_secs(15),
        }
    }
}
pub struct S3BlobStore {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
    runtime: Option<tokio::runtime::Runtime>,
    timeout: Duration,
}
// This cap covers zstd's bounded overhead while preventing unbounded compressed input.
const MAX_FRAME: usize = MAX_OBJECT_SIZE as usize + 1024 * 1024;
impl S3BlobStore {
    pub fn connect(options: S3Options) -> Result<Self> {
        if options.bucket.is_empty()
            || options.timeout.is_zero()
            || options.timeout > Duration::from_secs(300)
        {
            return Err(invalid("invalid S3 bucket or timeout"));
        }
        let prefix = Path::parse(&options.prefix).map_err(|_| invalid("invalid S3 prefix"))?;
        if prefix.as_ref() != options.prefix || options.prefix.is_empty() {
            return Err(invalid("S3 prefix must be nonempty and canonical"));
        }
        if let Some(endpoint) = &options.endpoint {
            let url = reqwest::Url::parse(endpoint).map_err(|_| invalid("invalid S3 endpoint"))?;
            if !matches!(url.scheme(), "https" | "http")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(invalid("invalid S3 endpoint"));
            }
            if url.scheme() == "http" && !options.allow_plaintext {
                return Err(invalid("plaintext S3 requires explicit permission"));
            }
        }
        crate::blocking::run(move || {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let client = object_store::ClientOptions::new()
                .with_allow_http(options.allow_plaintext)
                .with_connect_timeout(Duration::from_secs(5))
                .with_timeout(options.timeout);
            let mut builder = AmazonS3Builder::from_env()
                .with_bucket_name(options.bucket)
                .with_client_options(client)
                .with_conditional_put(S3ConditionalPut::ETagMatch)
                .with_virtual_hosted_style_request(!options.path_style)
                .with_retry(object_store::RetryConfig {
                    max_retries: 2,
                    retry_timeout: options.timeout,
                    ..Default::default()
                });
            if let Some(endpoint) = options.endpoint {
                builder = builder.with_config(AmazonS3ConfigKey::S3Endpoint, endpoint);
            }
            if let Some(region) = options.region {
                builder = builder.with_region(region);
            }
            let store = Arc::new(builder.build().map_err(s3_error)?);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("kasu-s3")
                .build()
                .map_err(storage_error)?;
            Ok(Self {
                store,
                prefix,
                runtime: Some(runtime),
                timeout: options.timeout,
            })
        })
    }
    fn path(&self, id: ObjectId) -> Path {
        let hex = id.to_string();
        self.prefix.clone().join(&hex[..2]).join(&hex[2..])
    }
    fn run<T: Send>(
        &self,
        future: impl std::future::Future<Output = Result<T>> + Send,
    ) -> Result<T> {
        crate::blocking::run(|| {
            self.runtime
                .as_ref()
                .ok_or_else(|| storage_error("S3 runtime closed"))?
                .block_on(async {
                    tokio::time::timeout(self.timeout, future)
                        .await
                        .map_err(|_| storage_error("S3 operation timed out"))?
                })
        })
    }
    async fn read(&self, id: ObjectId) -> Result<Vec<u8>> {
        let result = self.store.get(&self.path(id)).await.map_err(|e| match e {
            object_store::Error::NotFound { .. } => Error::NotFound(id.to_string()),
            other => s3_error(other),
        })?;
        if result.meta.size > MAX_FRAME as u64 {
            return Err(invalid("S3 frame exceeds size limit"));
        }
        let expected = result.meta.size as usize;
        let mut stream = result.into_stream();
        let mut bytes = Vec::with_capacity(expected.min(1024 * 1024));
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(s3_error)?;
            if chunk.len() > MAX_FRAME.saturating_sub(bytes.len()) {
                return Err(invalid("S3 frame exceeds size limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != expected {
            return Err(invalid("truncated S3 frame"));
        }
        let raw = decompress(&bytes)?;
        if ObjectId::hash(&raw) != id {
            return Err(invalid("corrupt S3 object hash"));
        }
        Ok(bytes)
    }
}
impl BlobStore for S3BlobStore {
    fn get_compressed(&self, id: ObjectId) -> Result<Vec<u8>> {
        self.run(self.read(id))
    }
    fn put_compressed(&self, id: ObjectId, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_FRAME {
            return Err(invalid("S3 frame exceeds size limit"));
        }
        let raw = decompress(bytes)?;
        if ObjectId::hash(&raw) != id {
            return Err(invalid("object hash mismatch"));
        }
        self.run(async {
            match self
                .store
                .put_opts(
                    &self.path(id),
                    bytes.to_vec().into(),
                    PutMode::Create.into(),
                )
                .await
            {
                Ok(_) => Ok(()),
                Err(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ) => {
                    let existing = self.read(id).await?;
                    if decompress(&existing)? != raw {
                        return Err(Error::Collision(id.to_string()));
                    }
                    Ok(())
                }
                Err(error) => Err(s3_error(error)),
            }
        })
    }
}
impl Drop for S3BlobStore {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            let _ = crate::blocking::run(move || {
                drop(runtime);
                Ok(())
            });
        }
    }
}
fn s3_error(error: object_store::Error) -> Error {
    // Provider errors can contain signed URLs and credential endpoints.
    let class =
        match error {
            object_store::Error::NotFound { .. } => "not found",
            object_store::Error::PermissionDenied { .. } => "permission denied",
            object_store::Error::Unauthenticated { .. } => "unauthenticated",
            object_store::Error::NotSupported { .. }
            | object_store::Error::NotImplemented { .. } => "operation unsupported",
            object_store::Error::AlreadyExists { .. }
            | object_store::Error::Precondition { .. } => "precondition failed",
            _ => "request failed",
        };
    storage_error(format!("S3 {class}"))
}
