//! # relix-controller — Relix node daemon
//!
//! Thin binary entry point for a production Relix controller node.
//! Parses a single `--config <path>` (short: `-c`) flag pointing at a
//! TOML config file, initialises `tracing_subscriber` from the `RUST_LOG`
//! environment variable (defaulting to `info`), then delegates entirely to
//! [`relix_runtime::controller_runtime::run`]. All node behaviour — which
//! node type to start, which capabilities to advertise, how to connect to
//! the mesh — is determined by the runtime from the TOML config.
//!
//! This crate contains no logic of its own beyond argument parsing and
//! logging setup.

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
