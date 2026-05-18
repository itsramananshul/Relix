//! Relix controller daemon entry point. M1 skeleton — bootstrap config + identity load
//! only. M5 wires the transport, registry, and dispatch bridge into a running mesh.

use clap::Parser;
use std::path::PathBuf;

/// Command-line arguments for the controller daemon.
#[derive(Parser, Debug)]
#[command(name = "relix-controller", version, about = "Relix controller daemon")]
struct Args {
    /// Path to the controller config TOML (see `configs/`).
    #[arg(short, long)]
    config: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    tracing::info!(config = %args.config.display(), "relix-controller booting (M1 skeleton)");

    // M5 will: load config, load/generate identity, build manifest, start transport,
    // register capabilities per node-type modules in relix-runtime::nodes::*,
    // and enter the inbound RPC event loop.
    tracing::warn!("M1 skeleton: no transport started; see specs/alpha-simplifications.md");
    Ok(())
}
