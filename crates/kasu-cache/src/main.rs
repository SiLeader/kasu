use clap::Parser;
use kasu_cache::{Layer, MemoryStore, Service, runtime};
use std::sync::Arc;
#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: runtime::CommonArgs,
    #[arg(long, default_value = "127.0.0.1:50091")]
    listen: std::net::SocketAddr,
    #[arg(long,default_value_t=64*1024*1024)]
    memory_bytes: usize,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _telemetry = kasu_telemetry::init("kasu-cache")?;
    let args = Args::parse();
    let layer: Arc<Layer> = args.common.build(
        kasu_object::Layer::Cache,
        Arc::new(MemoryStore::new(args.memory_bytes)),
    )?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let metrics = if let Some(address) = args.common.metrics {
        let listener = tokio::net::TcpListener::bind(address).await?;
        tracing::info!(metrics=%listener.local_addr()?,"cache metrics listening");
        Some(tokio::spawn(runtime::metrics(layer.clone(), listener)))
    } else {
        None
    };
    tracing::info!(address=%listener.local_addr()?,"kasu-cache listening");
    let result = tonic::transport::Server::builder()
        .add_service(Service::new(layer).into_server())
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            async {
                let _ = tokio::signal::ctrl_c().await;
            },
        )
        .await;
    if let Some(metrics) = metrics {
        metrics.abort();
    }
    result?;
    Ok(())
}
