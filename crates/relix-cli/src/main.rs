//! relix-cli — developer and operator CLI.

mod capability;
mod flow_run;
mod identity;
mod mcp;
mod ops;
mod ping;
mod router;
mod task;
mod topology;

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
    /// Default method is `node.health`. `--peer` is a libp2p multiaddr.
    Ping {
        /// Target peer's libp2p multiaddr.
        #[arg(long)]
        peer: String,
        /// Path to caller's identity bundle.
        #[arg(long)]
        identity: PathBuf,
        /// Method to call. Default `node.health`.
        #[arg(long, default_value = "node.health")]
        method: String,
        /// Path to a 32-byte signing key used as the local libp2p PeerId.
        #[arg(long)]
        client_key: PathBuf,
    },
    /// Operate the Coordinator's durable Task ledger.
    ///
    /// Each subcommand dials the Coordinator peer once, runs through
    /// the full admission pipeline (identity → policy → handler →
    /// audit), and prints the result. The Coordinator persists Tasks
    /// across restarts; see `docs/coordinator.md`.
    Task {
        #[command(subcommand)]
        cmd: task::Cmd,
    },
    /// Inspect peer capability manifests (T4 P3).
    ///
    /// Dials one peer, calls `node.manifest`, and prints the
    /// descriptors. Same dial-and-call pattern as `ping`. Read-only;
    /// goes through the admission pipeline.
    Capability {
        #[command(subcommand)]
        cmd: capability::Cmd,
    },
    /// Inspect the mesh topology via the bridge.
    ///
    /// Hits the bridge's `GET /v1/topology` endpoint and prints
    /// one line per cached peer with freshness, capability count,
    /// and an at-a-glance `fresh` / `stale` / `expired` verdict.
    /// Use this to spot peers whose manifest-refresh loop has
    /// silently stalled.
    Topology {
        #[command(subcommand)]
        cmd: topology::Cmd,
    },
    /// PH-WAVE2L: operator ops snapshots. Currently just
    /// `providers-health` against the bridge's consolidated
    /// `/v1/providers/health` endpoint.
    Ops {
        #[command(subcommand)]
        cmd: ops::Cmd,
    },
    /// Operate the Router Node — mesh observability + health
    /// control plane. Each subcommand dials the router peer
    /// once, presents an identity bundle, and prints the
    /// response. The router never makes LLM calls and never
    /// holds provider keys.
    Router {
        #[command(subcommand)]
        cmd: router::Cmd,
    },
    /// PH-MCP-CLI: inspect the MCP registry on a tool node.
    /// `mcp servers` lists registered MCP servers + their
    /// declared status; `mcp tools <id>` lists tools a server
    /// has declared. Read-only; uses libp2p dial-and-call (no
    /// bridge proxy required).
    Mcp {
        #[command(subcommand)]
        cmd: mcp::Cmd,
    },
    /// Execute a SOL flow file against a real Relix mesh (M6).
    ///
    /// Compiles the flow, attaches a libp2p-backed `RemoteCallDispatcher`,
    /// dials every peer named in the `--peers` file, runs the VM, and
    /// prints the result + the flow log path.
    FlowRun {
        /// Path to the `.sol` source file.
        #[arg(long)]
        flow: PathBuf,
        /// Caller's identity bundle (from `relix-cli identity mint`).
        #[arg(long)]
        identity: PathBuf,
        /// 32-byte signing key used as the local libp2p PeerId AND as the
        /// signer for the per-flow event log.
        #[arg(long)]
        client_key: PathBuf,
        /// TOML file with `[peers.<alias>] addr = "..."` entries.
        #[arg(long)]
        peers: PathBuf,
        /// Per-call deadline in seconds (default 30).
        #[arg(long, default_value_t = 30)]
        deadline_secs: i64,
    },
}

#[tokio::main]
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
        Cmd::Task { cmd } => task::run(cmd).await,
        Cmd::Capability { cmd } => capability::run(cmd).await,
        Cmd::Topology { cmd } => topology::run(cmd).await,
        Cmd::Ops { cmd } => ops::run(cmd).await,
        Cmd::Router { cmd } => router::run(cmd).await,
        Cmd::Mcp { cmd } => mcp::run(cmd).await,
        Cmd::Ping {
            peer,
            identity,
            method,
            client_key,
        } => ping::run(&peer, &identity, &method, &client_key).await,
        Cmd::FlowRun {
            flow,
            identity,
            client_key,
            peers,
            deadline_secs,
        } => flow_run::run(&flow, &identity, &client_key, &peers, deadline_secs).await,
    }
}
