use kasu_server::{
    MetadataService, ObjectService, WebhookConfig, WebhookDispatcher, WriteService, http_router,
};
use std::{ffi::OsString, path::PathBuf, sync::Arc};

const USAGE: &str = "usage: kasu-server <kasu-directory> [repository-name] [grpc-address] [http-address]\n       kasu-server migrate-registry <kasu-directory> <repository-name>";

fn migrate(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let [root, name] = args else {
        return Err(USAGE.into());
    };
    let storage = kasu_store::Storage::from_env(&PathBuf::from(root))?;
    let name = kasu_core::RepoName::from_bytes(name.as_encoded_bytes())?;
    let record =
        kasu_server::migrate_registry(storage.database.as_ref(), &name, b"migrate-registry")
            .map_err(|error| error as Box<dyn std::error::Error>)?;
    println!("{} {}", record.name, record.key);
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _telemetry =
        kasu_telemetry::init("kasu-server").map_err(|e| e as Box<dyn std::error::Error>)?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "migrate-registry") {
        return migrate(&args[1..]);
    }
    if !(1..=4).contains(&args.len()) {
        return Err(USAGE.into());
    }
    let root = PathBuf::from(&args[0]);
    // Repository names never contain ':', so a listen address cannot be
    // mistaken for the legacy repository argument.
    let (repository, addresses) = match args.get(1) {
        Some(value) if !value.as_encoded_bytes().contains(&b':') => {
            (Some(value.as_encoded_bytes().to_vec()), &args[2..])
        }
        _ => (None, &args[1..]),
    };
    if addresses.len() > 2 {
        return Err(USAGE.into());
    }
    let kasu_server::ConfiguredBackend {
        backend,
        database: meta,
        legacy,
    } = kasu_server::load_backend(&root, repository)
        .map_err(|error| error as Box<dyn std::error::Error>)?;
    let backend = Arc::new(backend);
    let (shutdown, stop) = tokio::sync::watch::channel(false);
    let registry = tokio::spawn(
        backend
            .registry()
            .clone()
            .watch(std::time::Duration::from_millis(250), stop.clone()),
    );
    let dispatcher = if let Some(path) = std::env::var_os("KASU_WEBHOOKS_FILE") {
        let legacy = legacy.ok_or("KASU_WEBHOOKS_FILE configures the startup repository; pass its name or use KASU_REPOSITORIES_FILE")?;
        let config = WebhookConfig::load(&PathBuf::from(path))?;
        let scoped = Arc::new(backend.for_repository(&legacy.key)?);
        let dispatcher = Arc::new(WebhookDispatcher::new(scoped, meta, config)?);
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
    let grpc = address(addresses.first(), "127.0.0.1:50051")?;
    let http = tokio::net::TcpListener::bind(address(addresses.get(1), "127.0.0.1:50080")?).await?;
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
    registry.await?;
    result
}
