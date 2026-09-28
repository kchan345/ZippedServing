use std::{net::IpAddr, path::PathBuf, sync::Arc};

use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use zipped_file_serving::{Config, app};

#[derive(Parser)]
#[command(
    version,
    about = "Serve a local directory with uploads and streaming LZ4 downloads"
)]
struct Args {
    /// Existing directory to serve (including its subdirectories).
    directory: PathBuf,
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,
    #[arg(long, default_value_t = 8081)]
    port: u16,
    /// Shared compression worker count (default: logical CPUs, capped at 8).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=64))]
    compression_threads: Option<u16>,
    /// Maximum simultaneous downloads; excess requests receive HTTP 503.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=256))]
    max_downloads: u16,
    /// Maximum simultaneous uploads; excess requests receive HTTP 503.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=256))]
    max_uploads: u16,
    /// Maximum bytes per uploaded file (default: 100 GiB).
    #[arg(long, default_value_t = 107_374_182_400, value_parser = clap::value_parser!(u64).range(1..))]
    max_upload_bytes: u64,
    /// Maximum negotiated uncompressed chunk size in MiB.
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u16).range(1..=1024))]
    max_chunk_mib: u16,
    /// JSON profile storage outside the served tree (default: LOCALAPPDATA\\ZippedServing\\profiles.json).
    #[arg(long)]
    profiles_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let mut config = Config::new(args.directory)?;
    if let Some(threads) = args.compression_threads {
        config.compression_threads = usize::from(threads);
    }
    config.max_downloads = usize::from(args.max_downloads);
    config.max_uploads = usize::from(args.max_uploads);
    config.max_upload_bytes = args.max_upload_bytes;
    config.max_chunk_bytes = u64::from(args.max_chunk_mib) * 1024 * 1024;
    if let Some(file) = args.profiles_file {
        config.profiles_file = file;
    }
    let root = config.root.clone();
    let router = app(config)?;
    let listener = TcpListener::bind((args.bind, args.port)).await?;
    tracing::info!(address = %listener.local_addr()?, directory = %root.display(), "File server listening");
    tracing::warn!(
        "No authentication or TLS: use only on a trusted network. All clients can read files and create files/directories."
    );
    let stopping = Arc::new(tokio::sync::Notify::new());
    let signal = stopping.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => tracing::info!("Stopping server"),
            Err(error) => tracing::error!(%error, "Unable to listen for Ctrl+C; stopping server"),
        }
        signal.notify_one();
    });
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { stopping.notified().await })
        .await?;
    Ok(())
}
