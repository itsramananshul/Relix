//! relix-cli — developer and operator CLI.

mod identity;
mod ping;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "relix-cli", version, about = "Relix developer / operator CLI")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Identity management subcommands.
    Identity {
        #[command(subcommand)]
        cmd: identity::Cmd,
    },
    /// Call a peer's `node.health` capability and print the result.
    Ping {
        /// Target peer's libp2p multiaddr (e.g. `/ip4/127.0.0.1/tcp/9001`).
        peer_addr: String,
        /// Path to caller's identity bundle (raw CBOR).
        #[arg(long)]
        identity_bundle: PathBuf,
        /// Path to a 32-byte signing key used as the local libp2p PeerId
        /// (does NOT need to match the identity subject — alpha SIMP).
        #[arg(long)]
        client_key: PathBuf,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args = Args::parse();
    match args.cmd {
        Cmd::Identity { cmd } => identity::run(cmd),
        Cmd::Ping {
            peer_addr,
            identity_bundle,
            client_key,
        } => ping::run(&peer_addr, &identity_bundle, &client_key).await,
    }
}
