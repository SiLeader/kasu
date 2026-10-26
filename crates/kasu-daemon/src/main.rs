use clap::{Parser, Subcommand};
use kasu_cache::{DiskLimits, DiskStore, Service, runtime};
use kasu_daemon::{Lease, SocketLease};
use std::{os::unix::ffi::OsStrExt, path::PathBuf, sync::Arc};
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve {
        #[command(flatten)]
        common: Box<runtime::CommonArgs>,
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        snapshot: Option<PathBuf>,
        #[arg(long,default_value_t=8*1024*1024*1024)]
        disk_bytes: u64,
        #[arg(long, default_value_t = 100_000)]
        disk_entries: u64,
        #[arg(long, default_value_t = 2)]
        snapshot_keep: usize,
        #[arg(long,default_value_t=8*1024*1024*1024)]
        snapshot_max_bytes: u64,
        #[arg(long, default_value_t = 3600)]
        snapshot_interval_secs: u64,
    },
    Snapshot {
        #[arg(long)]
        data: PathBuf,
        /// Repositories to include (repeatable); none includes every repository.
        #[arg(long)]
        repo: Vec<std::ffi::OsString>,
        output: PathBuf,
    },
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _telemetry = kasu_telemetry::init("kasu-daemon")?;
    match Args::parse().command {
        Command::Snapshot { data, repo, output } => {
            let store = DiskStore::open(&data)?;
            let repositories: Vec<_> = repo.iter().map(|r| r.as_bytes().to_vec()).collect();
            println!("{}", store.export_snapshot_for(&output, &repositories)?);
        }
        Command::Serve {
            common,
            socket,
            data,
            snapshot,
            disk_bytes,
            disk_entries,
            snapshot_keep,
            snapshot_max_bytes,
            snapshot_interval_secs,
        } => {
            if snapshot_interval_secs > 31_536_000 {
                return Err("snapshot interval must be at most one year".into());
            }
            DiskLimits {
                bytes: disk_bytes,
                entries: disk_entries,
            }
            .validate()?;
            if !(1..=1000).contains(&snapshot_keep) {
                return Err("snapshot retention must be 1..1000".into());
            }
            std::fs::create_dir_all(&data)?;
            let _data = Lease::acquire(&data.join("daemon.lock"))?;
            let store = Arc::new(DiskStore::open_with_limits(
                &data,
                DiskLimits {
                    bytes: disk_bytes,
                    entries: disk_entries,
                },
            )?);
            store.prune_snapshots(snapshot_keep)?;
            if let Some(path) = snapshot {
                let count = store.import_snapshot(&path, snapshot_max_bytes)?;
                tracing::info!(objects = count, "daemon snapshot loaded");
            }
            let layer = common.build(kasu_object::Layer::Daemon, store.clone())?;
            let (listener, _socket) = SocketLease::bind(&socket)?;
            let metrics = if let Some(address) = common.metrics {
                let listener = tokio::net::TcpListener::bind(address).await?;
                tracing::info!(metrics=%listener.local_addr()?,"daemon metrics listening");
                Some(tokio::spawn(runtime::metrics(layer.clone(), listener)))
            } else {
                None
            };
            let snapshots = if snapshot_interval_secs > 0 {
                let store = store.clone();
                let path = data.join("snapshots");
                std::fs::create_dir_all(&path)?;
                Some(tokio::spawn(async move {
                    let mut timer = tokio::time::interval(std::time::Duration::from_secs(
                        snapshot_interval_secs,
                    ));
                    timer.tick().await;
                    loop {
                        timer.tick().await;
                        let store = store.clone();
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos();
                        let output = path.join(format!("kasu-snapshot-v1-{stamp}.tar"));
                        match kasu_telemetry::blocking(move || {
                            let count = store.export_snapshot(&output)?;
                            store.prune_snapshots(snapshot_keep)?;
                            Ok::<_, tonic::Status>(count)
                        })
                        .await
                        {
                            Ok(Ok(count)) => {
                                tracing::info!(objects = count, "daemon snapshot saved")
                            }
                            _ => tracing::warn!("daemon snapshot failed"),
                        }
                    }
                }))
            } else {
                None
            };
            tracing::info!(socket=%socket.display(),"kasu-daemon listening");
            let result = tonic::transport::Server::builder()
                .add_service(Service::new(layer).into_server())
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::UnixListenerStream::new(listener),
                    async {
                        let _ = tokio::signal::ctrl_c().await;
                    },
                )
                .await;
            if let Some(task) = metrics {
                task.abort();
            }
            if let Some(task) = snapshots {
                task.abort();
            }
            result?;
        }
    }
    Ok(())
}
