use kasu_server::{
    MetadataService, ObjectService, WebhookConfig, WebhookDispatcher, WriteService, http_router,
};
use std::{path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry =
        kasu_telemetry::init("kasu-server").map_err(|e| e as Box<dyn std::error::Error>)?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !(2..=4).contains(&args.len()) {
        return Err(
            "usage: kasu-server <kasu-directory> <repository-name> [grpc-address] [http-address]"
                .into(),
        );
    }
    let root = PathBuf::from(&args[0]);
    let kasu_server::ConfiguredBackend {
        backend,
        database: meta,
    } = kasu_server::load_backend(&root, args[1].as_encoded_bytes().to_vec())
        .map_err(|error| error as Box<dyn std::error::Error>)?;
    let backend = Arc::new(backend);
    let (shutdown, stop) = tokio::sync::watch::channel(false);
    let dispatcher = if let Some(path) = std::env::var_os("KASU_WEBHOOKS_FILE") {
        let config = WebhookConfig::load(&PathBuf::from(path))?;
        let dispatcher = Arc::new(WebhookDispatcher::new(backend.clone(), meta, config)?);
        Some(dispatcher)
    } else {
        None
    };
    fn address(
        value: Option<&std::ffi::OsString>,
        default: &str,
    ) -> Result<std::net::SocketAddr, Box<dyn std::error::Error>> {
        Ok(value
            .map(|s| s.to_str().ok_or("listen address must be ASCII"))
            .transpose()?
            .unwrap_or(default)
            .parse()?)
    }
    let grpc = address(args.get(2), "127.0.0.1:50051")?;
    let http = tokio::net::TcpListener::bind(address(args.get(3), "127.0.0.1:50080")?).await?;
    let grpc_listener = tokio::net::TcpListener::bind(grpc).await?;
    let dispatcher = dispatcher.map(|dispatcher| tokio::spawn(dispatcher.run(stop)));
    tracing::info!(grpc=%grpc_listener.local_addr()?,http=%http.local_addr()?,"kasu metadata server listening");
    let grpc_service = tonic::transport::Server::builder()
        .add_service(ObjectService::new(backend.clone()).into_server())
        .add_service(WriteService::new(backend.clone()).into_server())
        .add_service(MetadataService::new(backend.clone()).into_server());
    let result: Result<(), Box<dyn std::error::Error>> = tokio::select! {
        result=grpc_service.serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(grpc_listener))=>result.map_err(Into::into),
        result=axum::serve(http,http_router(backend).into_make_service_with_connect_info::<std::net::SocketAddr>())=>result.map_err(Into::into),
        result=tokio::signal::ctrl_c()=>result.map_err(Into::into),
    };
    let _ = shutdown.send(true);
    if let Some(dispatcher) = dispatcher {
        dispatcher.await?;
    }
    result
}
