//! relix-cli — developer and operator CLI.
//!
//! Subcommands (alpha set):
//! - `identity init-org` — generate org-root keypair (kept under `dev-keys/`).
//! - `identity mint` — sign an IdentityBundle for a subject.
//! - `identity inspect` — print bundle contents.
//! - `ping` — call a peer's `node.health` (M5+).

mod identity;

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
    /// Ping a peer's `node.health` capability. (M5+; M1 prints a stub message.)
    Ping {
        /// Peer node id (hex) or alias.
        peer: String,
        /// Path to the caller's identity bundle file.
        #[arg(long)]
        identity: Option<PathBuf>,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    match args.cmd {
        Cmd::Identity { cmd } => identity::run(cmd),
        Cmd::Ping { peer, identity } => {
            tracing::warn!(
                peer = %peer,
                identity = ?identity,
                "ping not yet wired (M5)"
            );
            Ok(())
        }
    }
}
