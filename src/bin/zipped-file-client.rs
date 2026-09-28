use std::{path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};
use zipped_file_serving::{
    client::{self, Options},
    transfer::Codec,
};

#[derive(Parser)]
#[command(
    version,
    about = "Streaming LZ4/zstd download, extraction, and verified chunk uploads"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
    /// Requested uncompressed chunk size; server may negotiate a smaller value.
    #[arg(long, global = true, default_value_t = 256, value_parser = clap::value_parser!(u16).range(1..=1024))]
    split_mib: u16,
    #[arg(long, global = true, value_enum, default_value_t = Codec::Lz4)]
    codec: Codec,
    /// Total output/input limit; archive mode also counts tar headers and padding.
    #[arg(long, global = true, default_value_t = 107_374_182_400, value_parser = clap::value_parser!(u64).range(1..))]
    max_bytes: u64,
    /// Maximum duration of each HTTP request, in seconds.
    #[arg(long, global = true, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_seconds: u64,
    /// LZ4 upload compression workers. Decompression remains streaming and serial.
    #[arg(long, global = true, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=64))]
    compression_threads: u16,
}

#[derive(Subcommand)]
enum Command {
    /// Download to a NEW local directory, publishing only after verification.
    Download {
        url: String,
        output: PathBuf,
        /// Stream-extract an ordinary tar.lz4/tar.zstd instead of negotiating chunks.
        #[arg(long)]
        archive: bool,
    },
    /// Upload a local file/directory to an /api/upload?path=destination URL.
    Upload { input: PathBuf, url: String },
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let options = Options {
        chunk_size: u64::from(args.split_mib) * 1024 * 1024,
        codec: args.codec,
        max_bytes: args.max_bytes,
        timeout: Duration::from_secs(args.timeout_seconds),
        compression_threads: usize::from(args.compression_threads),
    };
    let result = match args.command {
        Command::Download {
            url,
            output,
            archive,
        } => client::download(&url, &output, archive, &options),
        Command::Upload { input, url } => client::upload(&input, &url, &options),
    };
    match result {
        Ok(()) => {
            eprintln!("Transfer complete and verified.");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Transfer failed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
