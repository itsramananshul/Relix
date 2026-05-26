//! relix-cli — developer and operator CLI.

mod browser;
mod capability;
mod config;
mod doctor;
mod export;
mod flow_run;
mod fs;
mod identity;
mod mcp;
mod memory_inspect;
mod mesh;
mod ops;
mod os_secure;
mod ping;
mod router;
mod setup;
mod skills;
mod sol;
mod souls;
mod task;
mod terminal;
mod topology;
mod update;
mod web;

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
    /// PH-CLI-AUDIT-MIRRORS: filesystem operator surface.
    /// `fs audit` snapshots the per-jail mutation ring via the
    /// bridge's `GET /v1/fs/audit` proxy (PH-BRIDGE-FS-AUDIT).
    /// HTTP-against-bridge — no identity bundle required.
    Fs {
        #[command(subcommand)]
        cmd: fs::Cmd,
    },
    /// PH-CLI-WEB-BLOCKLIST: web-tool operator surface.
    /// `web blocklist` snapshots `[tool] blocked_hosts` via
    /// the bridge's `GET /v1/tool/blocklist` proxy
    /// (PH-DASH-BLOCKLIST). HTTP-against-bridge — no identity
    /// bundle required.
    Web {
        #[command(subcommand)]
        cmd: web::Cmd,
    },
    /// PH-CLI-BROWSER: browser-session operator surface.
    /// `browser sessions` lists currently-open
    /// `tool.browser.*` sessions via the bridge's
    /// `GET /v1/browser/sessions` proxy (PH-DASH-BROWSER).
    Browser {
        #[command(subcommand)]
        cmd: browser::Cmd,
    },
    /// W2-004a: SOL workflow authoring helpers.
    /// `sol templates` lists baked-in workflow templates;
    /// `sol new --template ping --out flows/my-ping.sol`
    /// writes one to disk for quick-add.
    Sol {
        #[command(subcommand)]
        cmd: sol::Cmd,
    },
    /// W2-008a: one-command environment health check. Hits
    /// the bridge's `/v1/health` and prints an opinionated
    /// PASS/WARN/FAIL report. Exits non-zero on any FAIL so
    /// CI / shell scripts can gate on it.
    Doctor(doctor::DoctorArgs),
    /// PH-TERM-CLI: inspect + control tool.terminal.* on a
    /// tool node. `terminal sessions` lists live runs;
    /// `terminal audit` snapshots the completion ring;
    /// `terminal cancel --session-id X` triggers cooperative
    /// cancel. Libp2p dial-and-call.
    Terminal {
        #[command(subcommand)]
        cmd: terminal::Cmd,
    },
    /// Guided interactive setup wizard.
    ///
    /// Prompts for AI provider + API key, optional messaging
    /// channels, and saves the result to `~/.relix/config.toml`.
    /// Run after install (the install scripts call this
    /// automatically); also runnable later to change provider /
    /// rotate keys / add channels. Re-running pre-fills every
    /// field from the existing config so an operator only has to
    /// change what's actually changing. `relix reconfigure` is a
    /// visible alias for the same flow.
    #[command(visible_alias = "reconfigure")]
    Setup,

    /// Boot the local Relix mesh.
    ///
    /// Wraps the platform-specific boot script
    /// (`scripts/relix-mesh-up.ps1` on Windows,
    /// `scripts/relix-mesh-up.sh` elsewhere). `--with-telegram`,
    /// `--with-discord`, `--with-slack`, and `--with-plugins`
    /// translate into the env vars those scripts already understand.
    /// Polls the bridge's `/health` until it returns 200, then opens
    /// the dashboard in the default browser unless `--no-browser`.
    Boot(mesh::BootArgs),

    /// Stop every running `relix-controller` and `relix-web-bridge`
    /// on this machine. Idempotent — exits 0 if nothing was running.
    Stop,

    /// Print bridge health + topology snapshot. Exits 1 if the bridge
    /// is unreachable, so this is safe to use as a CI / shell gate.
    Status(mesh::StatusArgs),

    /// Check for a newer Relix release. Hits the GitHub release API,
    /// compares against the running binary's version, and offers to
    /// download + replace if a newer version exists.
    Update(update::UpdateArgs),

    /// Export conversation history from Relix in JSON / Markdown / CSV.
    ///
    /// Specify exactly one scope: `--session <id>`, `--agent <name>`,
    /// or `--all`. The CLI calls `GET /v1/sessions/export` on the
    /// bridge; renderers + formats live on the bridge side so the
    /// output is the single source of truth.
    Export(export::ExportArgs),

    /// Manage SOUL.md persona files. `list` shows discovered
    /// soul files; `edit <agent>` opens the file in `$EDITOR`
    /// (creating it from a template if it doesn't exist).
    /// See `crates/relix-runtime/src/nodes/ai/soul.rs`.
    Souls {
        #[command(subcommand)]
        cmd: souls::Cmd,
    },

    /// Manage SKILL.md skill library. `list` shows every
    /// discovered skill (and any AGENTS.md the loader sees);
    /// `run <name>` prints the named skill's body so the
    /// operator can pipe it into their own runner. See
    /// `crates/relix-runtime/src/nodes/ai/skills.rs`.
    Skills {
        #[command(subcommand)]
        cmd: skills::Cmd,
    },

    /// Inspect the four-layer memory store
    /// (`memory.layered.db`). Subcommands: `list`, `show`,
    /// `search`, `invalidate`, `stats`. Talks to the bridge's
    /// `/v1/memory/records/*` and `/v1/memory/stats` endpoints
    /// — requires the bridge to have `[bridge] memory_db_path`
    /// configured.
    Memory {
        #[command(subcommand)]
        cmd: memory_inspect::Cmd,
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
        Cmd::Fs { cmd } => fs::run(cmd).await,
        Cmd::Web { cmd } => web::run(cmd).await,
        Cmd::Browser { cmd } => browser::run(cmd).await,
        Cmd::Sol { cmd } => sol::run(cmd).await,
        Cmd::Doctor(args) => doctor::run(args).await,
        Cmd::Terminal { cmd } => terminal::run(cmd).await,
        Cmd::Ping {
            peer,
            identity,
            method,
            client_key,
        } => ping::run(&peer, &identity, &method, &client_key).await,
        Cmd::Setup => setup::run(),
        Cmd::Boot(args) => mesh::boot(args).await,
        Cmd::Stop => mesh::stop(),
        Cmd::Status(args) => mesh::status(args).await,
        Cmd::Update(args) => update::run(args).await,
        Cmd::Export(args) => export::run(args).await,
        Cmd::Souls { cmd } => souls::run(cmd),
        Cmd::Skills { cmd } => skills::run(cmd),
        Cmd::Memory { cmd } => memory_inspect::run(cmd).await,
        Cmd::FlowRun {
            flow,
            identity,
            client_key,
            peers,
            deadline_secs,
        } => flow_run::run(&flow, &identity, &client_key, &peers, deadline_secs).await,
    }
}
