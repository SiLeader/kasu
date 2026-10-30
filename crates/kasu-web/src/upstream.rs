//! The gRPC client of kasu-server. Every request carries the delegate token
//! and the account it acts as; the server makes every decision.
use kasu_proto::v1::{
    admin_client::AdminClient, identity_client::IdentityClient, metadata_client::MetadataClient,
    review_client::ReviewClient, writer_client::WriterClient,
};
use std::{net::IpAddr, time::Duration};
use tonic::{Code, Status, metadata::MetadataValue, transport::Channel};

pub const READ_TIMEOUT: Duration = Duration::from_secs(10);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Who a request acts as, as kasu-server sees it.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    pub act_as: Option<String>,
    pub binding: Option<Vec<u8>>,
    /// The audit name of the browser user (proxy user or account).
    pub user: Option<String>,
    pub client: Option<IpAddr>,
}

#[derive(Clone)]
pub struct Upstream {
    channel: Channel,
    delegate: Option<String>,
}
impl Upstream {
    pub fn new(
        server: &str,
        delegate: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut endpoint = Channel::from_shared(server.to_owned())?
            .connect_timeout(Duration::from_secs(5))
            .tcp_nodelay(true);
        if server.starts_with("https://") {
            endpoint = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_webpki_roots())?;
        }
        Ok(Self {
            channel: endpoint.connect_lazy(),
            delegate,
        })
    }
    pub fn delegated(&self) -> bool {
        self.delegate.is_some()
    }
    /// A request acting as `caller`.
    pub fn request<T>(&self, caller: &Caller, message: T, write: bool) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        request.set_timeout(if write { WRITE_TIMEOUT } else { READ_TIMEOUT });
        let metadata = request.metadata_mut();
        let mut put = |name: &'static str, value: &str| {
            if let Ok(value) = MetadataValue::try_from(value) {
                metadata.insert(name, value);
            }
        };
        if let Some(token) = &self.delegate {
            put("authorization", &format!("Bearer {token}"));
            if let Some(account) = &caller.act_as {
                put("kasu-act-as", account);
            }
            if let Some(binding) = &caller.binding {
                put("kasu-act-binding", &hex::encode(binding));
            }
            if let Some(user) = &caller.user {
                // Header values are visible ASCII; others are dropped from audit.
                if user.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
                    put("kasu-act-user", user);
                }
            }
            if let Some(client) = caller.client {
                put("kasu-client-addr", &client.to_string());
            }
        }
        kasu_telemetry::inject(request.metadata_mut());
        request
    }
    /// A request authenticated with a PAT, for login only.
    pub fn with_token<T>(&self, token: &str, message: T) -> Result<tonic::Request<T>, Status> {
        let mut request = tonic::Request::new(message);
        request.set_timeout(READ_TIMEOUT);
        request.metadata_mut().insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {token}"))
                .map_err(|_| Status::unauthenticated("invalid token"))?,
        );
        Ok(request)
    }
    pub fn metadata(&self) -> MetadataClient<Channel> {
        MetadataClient::new(self.channel.clone())
            .max_decoding_message_size(256 * 1024 * 1024)
            .max_encoding_message_size(16 * 1024 * 1024)
    }
    pub fn writer(&self) -> WriterClient<Channel> {
        WriterClient::new(self.channel.clone())
            .max_decoding_message_size(64 * 1024 * 1024)
            .max_encoding_message_size(16 * 1024 * 1024)
    }
    pub fn admin(&self) -> AdminClient<Channel> {
        AdminClient::new(self.channel.clone())
    }
    pub fn identity(&self) -> IdentityClient<Channel> {
        IdentityClient::new(self.channel.clone())
    }
    pub fn archive(&self) -> kasu_proto::v1::archive_client::ArchiveClient<Channel> {
        kasu_proto::v1::archive_client::ArchiveClient::new(self.channel.clone())
    }
    pub fn review(&self) -> ReviewClient<Channel> {
        ReviewClient::new(self.channel.clone()).max_decoding_message_size(64 * 1024 * 1024)
    }
}
/// Run a read, retrying once when the server is unavailable.
pub async fn read<T, F, Fut>(mut call: F) -> Result<T, Status>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<tonic::Response<T>, Status>>,
{
    match call().await {
        Err(status) if status.code() == Code::Unavailable => Ok(call().await?.into_inner()),
        result => Ok(result?.into_inner()),
    }
}
