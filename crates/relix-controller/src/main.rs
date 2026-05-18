//! Relix controller daemon. Thin wrapper around `relix_runtime::controller_runtime::run`.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "relix-controller", version, about = "Relix controller daemon")]
struct Args {
    /// Path to the controller config TOML (see `configs/`).
    #[arg(short, long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    relix_runtime::controller_runtime::run(&args.config).await
}
