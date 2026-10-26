use kasu_core::{Canonical, ObjectId, Tree};
use kasu_proto::v1::{self as p, objects_client::ObjectsClient};
use tonic::{
    Request, Status,
    metadata::{Ascii, MetadataValue},
    transport::{Channel, Endpoint},
};
use tracing::{Instrument, instrument::WithSubscriber};

pub const CHUNK_BYTES: usize = 64 * 1024;
pub const BATCH_LIMIT: usize = 128;
pub const MAX_FRAME_BYTES: usize = kasu_store::MAX_COMPRESSED_OBJECT_SIZE;
pub const LAYER_HEADER: &str = "x-kasu-object-layer";
/// Opaque, stable identity of the repository a response was served for. It
/// changes when a name is reused by a different repository.
pub const REPOSITORY_HEADER: &str = "x-kasu-repository";
pub fn token_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(a.ct_eq(b))
}
pub fn authorized(expected: &MetadataValue<Ascii>, actual: Option<&MetadataValue<Ascii>>) -> bool {
    actual.is_some_and(|actual| token_eq(expected.as_encoded_bytes(), actual.as_encoded_bytes()))
}
pub fn read_token(path: &std::path::Path) -> Result<String, Status> {
    use std::io::Read;
    let mut bytes = vec![];
    std::fs::File::open(path)
        .map_err(|_| Status::invalid_argument("cannot read token file"))?
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| Status::invalid_argument("cannot read token file"))?;
    if bytes.len() > 4096 {
        return Err(Status::invalid_argument("token file exceeds limit"));
    }
    let token =
        String::from_utf8(bytes).map_err(|_| Status::invalid_argument("token must be UTF-8"))?;
    let token = token.trim_end_matches(['\r', '\n']).to_owned();
    bearer(&token)?;
    Ok(token)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Tree,
    Blob,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tree => "trees",
            Self::Blob => "blobs",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Server,
    Cache,
    Daemon,
}
impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Cache => "cache",
            Self::Daemon => "daemon",
        }
    }
}
pub fn bearer(token: &str) -> Result<MetadataValue<Ascii>, Status> {
    if token.is_empty() || token.len() > 4096 {
        return Err(Status::invalid_argument("empty object token"));
    }
    let mut value: MetadataValue<Ascii> = format!("Bearer {token}")
        .parse()
        .map_err(|_| Status::invalid_argument("invalid object token"))?;
    // Preserve redaction through request cloning and conversion to HTTP headers.
    value.set_sensitive(true);
    Ok(value)
}
pub async fn connect(endpoint: &str) -> Result<Channel, Status> {
    let error = |_: String| Status::unavailable("object endpoint connection failed");
    if let Some(path) = endpoint.strip_prefix("unix://") {
        if !std::path::Path::new(path).is_absolute() {
            return Err(Status::invalid_argument(
                "Unix socket path must be absolute",
            ));
        }
        let path = std::path::PathBuf::from(path);
        return Endpoint::from_static("http://[::]:50051")
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect_with_connector(tower::service_fn(move |_| {
                let path = path.clone();
                async move {
                    tokio::net::UnixStream::connect(path)
                        .await
                        .map(hyper_util::rt::TokioIo::new)
                }
            }))
            .await
            .map_err(|e| error(e.to_string()));
    }
    http_endpoint(endpoint)?
        .connect()
        .await
        .map_err(|e| error(e.to_string()))
}
fn http_endpoint(endpoint: &str) -> Result<Endpoint, Status> {
    let endpoint = Endpoint::from_shared(endpoint.to_owned())
        .map_err(|_| Status::invalid_argument("invalid object endpoint"))?;
    if !matches!(endpoint.uri().scheme_str(), Some("http" | "https"))
        || endpoint
            .uri()
            .authority()
            .is_none_or(|a| a.as_str().contains('@'))
    {
        return Err(Status::invalid_argument(
            "endpoint must be HTTP(S) without credentials, or unix://",
        ));
    }
    let endpoint = if endpoint.uri().scheme_str() == Some("https") {
        endpoint
            .tls_config(tonic::transport::ClientTlsConfig::new().with_webpki_roots())
            .map_err(|_| Status::invalid_argument("invalid object TLS configuration"))?
    } else {
        endpoint
    };
    Ok(endpoint.connect_timeout(std::time::Duration::from_secs(5)))
}
pub fn lazy(endpoint: &str) -> Result<Channel, Status> {
    if let Some(path) = endpoint.strip_prefix("unix://") {
        if !std::path::Path::new(path).is_absolute() {
            return Err(Status::invalid_argument(
                "Unix socket path must be absolute",
            ));
        }
        let path = std::path::PathBuf::from(path);
        Ok(Endpoint::from_static("http://[::]:50051")
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect_with_connector_lazy(tower::service_fn(move |_| {
                let path = path.clone();
                async move {
                    tokio::net::UnixStream::connect(path)
                        .await
                        .map(hyper_util::rt::TokioIo::new)
                }
            })))
    } else {
        Ok(http_endpoint(endpoint)?.connect_lazy())
    }
}
#[derive(Clone)]
pub struct Client {
    client: ObjectsClient<Channel>,
    authorization: Option<MetadataValue<Ascii>>,
    expected: Option<Layer>,
}
impl Client {
    pub fn expected_layer(&self) -> Option<Layer> {
        self.expected
    }
    pub fn uses_token(&self, token: &str) -> bool {
        bearer(token).is_ok_and(|token| self.authorization.as_ref() == Some(&token))
    }
    pub fn new(
        channel: Channel,
        token: Option<&str>,
        expected: Option<Layer>,
    ) -> Result<Self, Status> {
        Ok(Self {
            client: ObjectsClient::new(channel).max_decoding_message_size(CHUNK_BYTES + 1024),
            authorization: token.map(bearer).transpose()?,
            expected,
        })
    }
    pub async fn connect(
        endpoint: &str,
        token: Option<&str>,
        expected: Option<Layer>,
    ) -> Result<Self, Status> {
        Self::new(connect(endpoint).await?, token, expected)
    }
    fn request(&self, repo: &[u8], id: ObjectId) -> Request<p::ObjectRequest> {
        let mut q = Request::new(p::ObjectRequest {
            repo: repo.to_vec(),
            ids: vec![id.0.to_vec()],
        });
        if let Some(value) = &self.authorization {
            q.metadata_mut().insert("authorization", value.clone());
        }
        kasu_telemetry::inject(q.metadata_mut());
        q
    }
    fn check<T>(&self, response: &tonic::Response<T>) -> Result<(), Status> {
        if let Some(expected) = self.expected
            && response
                .metadata()
                .get(LAYER_HEADER)
                .and_then(|v| v.to_str().ok())
                != Some(expected.name())
        {
            return Err(Status::failed_precondition(
                "object hierarchy layer mismatch",
            ));
        }
        Ok(())
    }
    pub async fn fetch(&self, repo: &[u8], id: ObjectId, kind: Kind) -> Result<Vec<u8>, Status> {
        self.fetch_scoped(repo, id, kind)
            .await
            .map(|(bytes, _)| bytes)
    }
    /// Fetch one frame and the repository identity the upstream reported.
    pub async fn fetch_scoped(
        &self,
        repo: &[u8],
        id: ObjectId,
        kind: Kind,
    ) -> Result<(Vec<u8>, Option<String>), Status> {
        let identity = |metadata: &tonic::metadata::MetadataMap| {
            metadata
                .get(REPOSITORY_HEADER)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 128)
                .map(str::to_owned)
        };
        let span = kasu_telemetry::client_span(
            if kind == Kind::Tree {
                "GetTrees"
            } else {
                "GetBlobs"
            },
            "grpc",
        );
        let result = async {
            let mut client = self.client.clone();
            let q = self.request(repo, id);
            let mut frame = Frame::new(id);
            match kind {
                Kind::Tree => {
                    let response = client.get_trees(q).await?;
                    self.check(&response)?;
                    let scope = identity(response.metadata());
                    let mut stream = response.into_inner();
                    while let Some(chunk) = stream.message().await? {
                        frame.push(&chunk.id, chunk.offset, &chunk.data, chunk.last)?;
                    }
                    Ok((frame.finish()?, scope))
                }
                Kind::Blob => {
                    let response = client.get_blobs(q).await?;
                    self.check(&response)?;
                    let scope = identity(response.metadata());
                    let mut stream = response.into_inner();
                    while let Some(chunk) = stream.message().await? {
                        frame.push(&chunk.id, chunk.offset, &chunk.data, chunk.last)?;
                    }
                    Ok((frame.finish()?, scope))
                }
            }
        }
        .instrument(span.clone())
        .with_current_subscriber()
        .await;
        kasu_telemetry::finish(
            &span,
            result.as_ref().err().map_or(tonic::Code::Ok, Status::code),
        );
        result
    }
}
pub struct Frame {
    id: ObjectId,
    bytes: Vec<u8>,
    complete: bool,
}
impl Frame {
    pub fn new(id: ObjectId) -> Self {
        Self {
            id,
            bytes: vec![],
            complete: false,
        }
    }
    pub fn push(&mut self, id: &[u8], offset: u64, data: &[u8], last: bool) -> Result<(), Status> {
        if self.complete
            || id != self.id.0
            || offset != self.bytes.len() as u64
            || data.is_empty()
            || data.len() > CHUNK_BYTES
            || self.bytes.len().saturating_add(data.len()) > MAX_FRAME_BYTES
        {
            return Err(Status::data_loss("invalid object chunk sequence"));
        }
        self.bytes.extend_from_slice(data);
        self.complete = last;
        Ok(())
    }
    pub fn finish(self) -> Result<Vec<u8>, Status> {
        if self.complete {
            Ok(self.bytes)
        } else {
            Err(Status::data_loss("incomplete object frame"))
        }
    }
}
pub fn validate(kind: Kind, id: ObjectId, bytes: &[u8]) -> Result<(), Status> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Status::resource_exhausted(
            "compressed object exceeds limit",
        ));
    }
    let raw = kasu_store::decompress(bytes)
        .map_err(|_| Status::data_loss("invalid compressed object"))?;
    if ObjectId::hash(&raw) != id {
        return Err(Status::data_loss("object hash mismatch"));
    }
    if kind == Kind::Tree {
        Tree::decode(&raw).map_err(|_| Status::data_loss("invalid canonical tree"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_credentials_are_redacted_in_requests_and_http_headers() {
        let secret = "regression-secret-token";
        let value = bearer(secret).unwrap();
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("authorization", value.clone());
        assert!(!format!("{request:?}").contains(secret));
        let headers = request.metadata().clone().into_headers();
        assert!(!format!("{headers:?}").contains(secret));
        assert!(headers["authorization"].is_sensitive());
        assert_eq!(value.to_str().unwrap(), format!("Bearer {secret}"));
        assert!(authorized(&value, Some(&value.clone())));
    }
}
