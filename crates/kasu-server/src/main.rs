use kasu_server::{
    AdminService, ArchiveService, ConfigSource, IdentityService, MetadataService, ObjectService,
    ReviewService, WebhookConfig, WebhookSupervisor, WriteService, http_router,
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
        ..
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
    let source = ConfigSource::from_env()?;
    let declarations = std::env::var_os("KASU_REPOSITORIES_FILE").map(PathBuf::from);
    if let Some(path) = &declarations {
        let declared = kasu_server::load_repositories_file(path)?;
        let report = kasu_server::sync_repositories(
            backend.registry().store().as_ref(),
            &declared,
            source,
            now(),
        )?;
        tracing::info!(?report, "repositories file synchronized");
    }
    let declarations = declarations.map(|path| {
        tokio::spawn(follow_repositories_file(
            backend.clone(),
            path,
            source,
            stop.clone(),
        ))
    });
    let mut settings = match std::env::var_os("KASU_WEBHOOKS_FILE") {
        Some(path) => WebhookConfig::load(&PathBuf::from(path))?,
        None => WebhookConfig::default(),
    };
    if let Ok(url) = std::env::var("KASU_PUBLIC_URL") {
        settings.public_url = url;
    }
    let supervisor = Arc::new(WebhookSupervisor::new(backend.clone(), meta, settings)?);
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
    let purge_interval = std::env::var("KASU_PURGE_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| (100..=86_400_000).contains(v))
        .unwrap_or(60_000);
    let purge = tokio::spawn(backend.clone().purge_loop(
        std::time::Duration::from_millis(purge_interval),
        stop.clone(),
    ));
    let dispatcher = tokio::spawn(supervisor.run(stop));
    tracing::info!(grpc=%grpc_listener.local_addr()?,http=%http.local_addr()?,"kasu metadata server listening");
    let grpc_service = tonic::transport::Server::builder()
        .add_service(ObjectService::new(backend.clone()).into_server())
        .add_service(WriteService::new(backend.clone()).into_server())
        .add_service(MetadataService::new(backend.clone()).into_server())
        .add_service(AdminService::new(backend.clone()).into_server())
        .add_service(IdentityService::new(backend.clone()).into_server())
        .add_service(ReviewService::new(backend.clone()).into_server())
        .add_service(ArchiveService::new(backend.clone()).into_server());
    let result: Result<(), Box<dyn std::error::Error>> = tokio::select! {
        result=grpc_service.serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(grpc_listener))=>result.map_err(Into::into),
        result=axum::serve(http,http_router(backend).into_make_service_with_connect_info::<std::net::SocketAddr>())=>result.map_err(Into::into),
        result=tokio::signal::ctrl_c()=>result.map_err(Into::into),
    };
    let _ = shutdown.send(true);
    dispatcher.await?;
    purge.await?;
    registry.await?;
    if let Some(task) = declarations {
        task.await?;
    }
    result
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
/// Re-read the declaration on SIGHUP and on a polling interval. A file that
/// fails to parse keeps the current registry state.
async fn follow_repositories_file(
    backend: Arc<kasu_server::Backend>,
    path: PathBuf,
    source: ConfigSource,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let interval = std::env::var("KASU_REPOSITORIES_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| (100..=3_600_000).contains(v))
        .unwrap_or(30_000);
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(interval)) => {}
            _ = async { match &mut hangup { Some(s) => { s.recv().await; } None => std::future::pending().await } } => {}
            _ = shutdown.changed() => return,
        }
        let backend = backend.clone();
        let path = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let declared = kasu_server::load_repositories_file(&path)?;
            kasu_server::sync_repositories(
                backend.registry().store().as_ref(),
                &declared,
                source,
                now(),
            )
        })
        .await;
        match result {
            Ok(Ok(report))
                if report
                    != kasu_server::SyncReport {
                        unchanged: report.unchanged,
                        undeclared: report.undeclared,
                        ..Default::default()
                    } =>
            {
                tracing::info!(?report, "repositories file synchronized")
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(message=%error.message(),"repositories file was not applied")
            }
            Err(_) => tracing::error!("repositories file worker failed"),
        }
    }
}
