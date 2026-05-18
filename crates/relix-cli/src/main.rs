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
    /// Call a peer's capability and print the response.
    ///
    /// Default method is `node.health`. For the alpha, `--peer` is a libp2p
    /// multiaddr (e.g. `/ip4/127.0.0.1/tcp/9001`). Alias-based dialing lands
    /// when capability gossip arrives at M6+.
    Ping {
        /// Target peer's libp2p multiaddr.
        #[arg(long)]
        peer: String,
        /// Path to caller's identity bundle (raw CBOR from `relix-cli identity mint`).
        #[arg(long)]
        identity: PathBuf,
        /// Method to call. Default `node.health`.
        #[arg(long, default_value = "node.health")]
        method: String,
        /// Path to a 32-byte signing key used as the local libp2p PeerId.
        /// (Does NOT need to match the identity subject — alpha SIMP.)
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
            peer,
            identity,
            method,
            client_key,
        } => ping::run(&peer, &identity, &method, &client_key).await,
    }
}
