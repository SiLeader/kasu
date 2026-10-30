#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _telemetry = kasu_telemetry::init("kasu-web")?;
    if std::env::args_os().len() > 1 {
        return Err("usage: kasu-web (configured with KASU_WEB_* environment variables)".into());
    }
    let config = kasu_web::Config::from_env()?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let app = kasu_web::App::new(config)?;
    tracing::info!(address = %listener.local_addr()?, "kasu web UI listening");
    axum::serve(
        listener,
        kasu_web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
