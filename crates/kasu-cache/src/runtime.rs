use crate::{Layer, Limits};
use kasu_object::{Client, Layer as Role};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tonic::Status;
#[derive(clap::Args)]
pub struct CommonArgs {
    #[arg(long)]
    pub repo: std::ffi::OsString,
    #[arg(long)]
    pub upstream: String,
    #[arg(long, env = "KASU_LAYER_TOKEN_FILE")]
    pub token_file: PathBuf,
    #[arg(long, env = "KASU_UPSTREAM_TOKEN_FILE")]
    pub upstream_token_file: PathBuf,
    #[arg(long, default_value_t = 16)]
    pub streams: usize,
    #[arg(long, default_value_t = 2)]
    pub fills: usize,
    #[arg(long, default_value_t = 128)]
    pub pending: usize,
    #[arg(long, default_value_t = 20)]
    pub qps: u32,
    #[arg(long, default_value_t = 1000)]
    pub queue_timeout_ms: u64,
    #[arg(long, default_value_t = 5000)]
    pub upstream_timeout_ms: u64,
    #[arg(long, default_value_t = 1000)]
    pub cooldown_ms: u64,
    #[arg(long)]
    pub metrics: Option<std::net::SocketAddr>,
}
impl CommonArgs {
    pub fn build(&self, role: Role, store: Arc<dyn crate::Store>) -> Result<Arc<Layer>, Status> {
        use std::os::unix::ffi::OsStrExt;
        let token = read_token(&self.token_file)?;
        let upper = read_token(&self.upstream_token_file)?;
        let expected = match role {
            Role::Cache => Role::Server,
            Role::Daemon => Role::Cache,
            Role::Server => return Err(Status::invalid_argument("invalid layer role")),
        };
        let client = Client::new(
            kasu_object::lazy(&self.upstream)?,
            Some(&upper),
            Some(expected),
        )?;
        Layer::new(
            role,
            self.repo.as_bytes().to_vec(),
            &token,
            &upper,
            client,
            store,
            Limits {
                streams: self.streams,
                fills: self.fills,
                pending: self.pending,
                qps: self.qps,
                queue_timeout: Duration::from_millis(self.queue_timeout_ms),
                upstream_timeout: Duration::from_millis(self.upstream_timeout_ms),
                cooldown: Duration::from_millis(self.cooldown_ms),
                ..Default::default()
            },
        )
    }
}
pub use kasu_object::read_token;
pub async fn metrics(layer: Arc<Layer>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    let router = axum::Router::new()
        .route(
            "/metrics",
            axum::routing::get(
                |axum::extract::State(layer): axum::extract::State<Arc<Layer>>| async move {
                    (
                        [("content-type", "text/plain; version=0.0.4")],
                        layer.metrics(),
                    )
                },
            ),
        )
        .with_state(layer);
    axum::serve(listener, router).await
}
