//! # relix-cli — developer and operator CLI
//!
//! Multi-subcommand binary for interacting with a live Relix mesh from the
//! command line. Every subcommand dials a peer's libp2p multiaddr, runs
//! through the full admission pipeline (identity → policy → handler →
//! audit), and prints the result. No subcommand owns persistent state.
//!
//! Key subcommand groups: `identity` (issue/verify bundles), `ping`
//! (health + capability probe), `task` (Coordinator ledger), `capability`
//! (manifest inspection), `topology` (mesh peer table via the bridge),
//! `memory` (direct memory-node queries), `workflow` (SOL flow
//! list/validate/run/trace), and many more — run `relix-cli --help` for
//! the full list. Operator-facing subcommands require a valid identity
//! bundle (`--identity`) and a 32-byte signing key (`--client-key`).

mod approval;
mod belief;
mod bridge_token;
mod browser;
mod build;
mod call;
mod capability;
mod confidence;
mod config;
mod credentials;
mod dashboard;
mod defaults;
mod doctor;
mod email;
mod eval;
mod execution;
mod export;
mod flow;
mod flow_run;
mod fs;
mod identity;
mod install;
mod judge;
mod knowledge;
mod mcp;
mod memory_inspect;
mod mesh;
mod metrics;
mod models;
mod observe;
mod ops;
mod os_secure;
mod pii;
mod ping;
mod planning;
mod provenance;
mod reasoning;
mod release;
mod router;
mod routing;
mod secret_input;
mod sessions;
mod setup;
mod skills;
mod sol;
mod souls;
mod task;
mod terminal;
mod tool;
mod topology;
mod training;
mod update;
mod web;
mod workflow;

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
    /// Invoke ANY capability by wire name with a verbatim argument
    /// string (the operator escape hatch for capabilities without a
    /// bespoke subcommand — `brief.*`, `mandate.*`, `agent.*`, …).
    Call(call::CallArgs),
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
    /// PH-WAVE2L: operator ops snapshots. `capabilities`,
    /// `stuck`, `dispatch-stats`, `smoke`, `policy-simulate`,
    /// `policy-denials`, `events`, and `cron` subcommands
    /// against the bridge's `/v1/*` surface. See `ops::Cmd` for
    /// the full list.
    Ops {
        #[command(subcommand)]
        cmd: ops::Cmd,
    },
    /// RELIX-7.7 email channel operator surface.
    /// `email send`   — one-off `POST /v1/email/send`.
    /// `email status` — SMTP + IMAP connection state.
    /// `email test`   — self-test: send a probe email to the
    /// configured `smtp_from`. Talks to the local bridge over
    /// HTTP (default `http://127.0.0.1:19791`).
    Email {
        #[command(subcommand)]
        cmd: email::Cmd,
    },
    /// RELIX-7.11 agent performance metrics surface.
    /// `metrics summary [--agent X] [--hours N]` — tabular view.
    /// `metrics alerts` — active alerts with severity badges.
    /// `metrics cost [--hours N]` — cost by (agent, method).
    /// `metrics timeseries --agent X [--hours 6] [--bucket 5]` —
    /// ASCII sparkline of invocation rate. Talks to the local
    /// bridge over HTTP (default `http://127.0.0.1:19791`).
    Metrics {
        #[command(subcommand)]
        cmd: metrics::Cmd,
    },
    /// RELIX-7.28 Part 2 — live observability dashboard.
    /// `relix observe` runs a refreshing terminal UI; `--once`
    /// prints one snapshot; `--alerts` / `--health` render
    /// individual panels for scripting.
    Observe(observe::ObserveArgs),
    /// RELIX-7.28 Part 3 — mesh-level PII detection surface.
    /// `relix pii stats [--hours N]` and
    /// `relix pii events [--method M] [--limit N]`.
    Pii {
        #[command(subcommand)]
        cmd: pii::Cmd,
    },
    /// RELIX-7.15 training data pipeline surface.
    /// `training stats` — aggregate counters + score histogram.
    /// `training list [--agent X] [--min-quality 0.7] [--limit 20]`
    /// — recent interactions.
    /// `training show <id>` — full record.
    /// `training export --format openai --set-name <name> --output <dir>`
    /// — runs an export.
    /// `training delete <id>` — hard-deletes one interaction.
    /// Talks to the local bridge over HTTP (default
    /// `http://127.0.0.1:19791`).
    Training {
        #[command(subcommand)]
        cmd: training::Cmd,
    },
    /// RELIX-7.16 agent-to-agent knowledge transfer.
    /// `knowledge groups` lists configured sharing groups.
    /// `knowledge share --from X --to A,B --ids id1,id2`
    /// copies observations from one agent to one or more.
    /// `knowledge broadcast --group G --caller X --ids id1,id2`
    /// broadcasts to a group.
    /// `knowledge shared --agent X` lists what an agent received.
    /// `knowledge revoke --ids id1,id2` soft-deletes a received
    /// observation.
    /// Talks to the local bridge over HTTP (default
    /// `http://127.0.0.1:19791`).
    Knowledge {
        #[command(subcommand)]
        cmd: knowledge::Cmd,
    },
    /// RELIX-7.19: per-step confidence scoring + fallback
    /// surface.
    /// `confidence policies` prints every configured policy.
    /// `confidence history --agent X --method ai.chat` prints
    /// the rolling window snapshot for a (agent, method) pair.
    /// `confidence reset --agent X [--method M]` clears the
    /// window. Talks to the local bridge over HTTP (default
    /// `http://127.0.0.1:19791`).
    Confidence {
        #[command(subcommand)]
        cmd: confidence::Cmd,
    },
    /// RELIX-7.29 PART 3: belief tracker inspector. `belief
    /// show --session <id>` prints the LLM-driven belief list
    /// the AI handler has accumulated for a session. `belief
    /// reset --session <id>` clears it. `--subject` defaults
    /// to the bridge identity's subject. Talks to the local
    /// bridge over HTTP (default `http://127.0.0.1:19791`).
    Belief {
        #[command(subcommand)]
        cmd: belief::Cmd,
    },
    /// RELIX-7.30 PART 1: out-of-band approval delivery
    /// inspector. `approval delivery-status <id>` prints the
    /// matched channel + escalation state + operator decision
    /// (if any) for one approval id. Talks to the local
    /// bridge over HTTP (default `http://127.0.0.1:19791`).
    Approval {
        #[command(subcommand)]
        cmd: approval::Cmd,
    },
    /// RELIX-7.30 PART 2: credential vault. `credentials store
    /// --name <n> --value <v>` encrypts + persists; `list`,
    /// `rotate`, `revoke`, `audit` exercise the lifecycle.
    /// Talks to the local bridge over HTTP (default
    /// `http://127.0.0.1:19791`).
    Credentials {
        #[command(subcommand)]
        cmd: credentials::Cmd,
    },
    /// RELIX-7.29 PART 4: judge model inspector. `judge
    /// verdicts [--limit N]` prints the rolling ring of judge
    /// verdicts; `judge stats` prints proceed/modify/block/
    /// timeout counters with a per-agent breakdown. Talks to
    /// the local bridge over HTTP (default
    /// `http://127.0.0.1:19791`).
    Judge {
        #[command(subcommand)]
        cmd: judge::Cmd,
    },
    /// RELIX-7.29 PART 5: full §7.29 reasoning-engine status.
    /// `reasoning status` prints a per-component summary
    /// (routing, self_consistency, belief_state, judge) so
    /// operators can see at a glance which of the four
    /// components are configured + live. Talks to the local
    /// bridge over HTTP (default `http://127.0.0.1:19791`).
    Reasoning {
        #[command(subcommand)]
        cmd: reasoning::Cmd,
    },
    /// RELIX-7.29 PART 1: smart-routing inspector.
    /// `routing explain --message "<text>" [--session-turns N]`
    /// classifies a message with the §7.29 ComplexityClassifier
    /// and prints what the coordinator's tier router would pick
    /// for it. Talks to the local bridge over HTTP (default
    /// `http://127.0.0.1:19791`).
    Routing {
        #[command(subcommand)]
        cmd: routing::Cmd,
    },
    /// RELIX-7.24: spec-driven multi-agent planning pipeline.
    /// `planning agents` lists the agents visible to the
    /// coordinator's capability registry. `planning search
    /// --task "..."` scores the registry against a task.
    /// `planning validate --spec "..."` parses a spec into
    /// the structured PlanSpec. `planning plan --spec "..."`
    /// generates a workflow (and optionally executes it with
    /// `--execute`). Talks to the local bridge over HTTP
    /// (default `http://127.0.0.1:19791`).
    Planning {
        #[command(subcommand)]
        cmd: planning::Cmd,
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
    /// Workflow scaffolding helpers.
    /// `flow yaml` prints a minimal YAML flow template to
    /// stdout — pipe it into a file, edit peer/method/arg,
    /// then run with `relix-cli flow-run`.
    Flow {
        #[command(subcommand)]
        cmd: flow::Cmd,
    },
    /// Multi-agent workflow engine (RELIX-7.5).
    /// `workflow list` shows the catalog, `workflow run
    /// <name> --input <text>` executes one, `workflow
    /// validate <file>` type-checks a `.workflow` source,
    /// `workflow trace <execution-id>` looks up a past run.
    /// Talks to the local bridge over HTTP (default
    /// `http://127.0.0.1:19791`); override with `--bridge`.
    Workflow {
        #[command(subcommand)]
        cmd: workflow::Cmd,
    },
    /// W2-008a: one-command environment health check. Hits
    /// the bridge's `/v1/health` and prints an opinionated
    /// PASS/WARN/FAIL report. Exits non-zero on any FAIL so
    /// CI / shell scripts can gate on it.
    Doctor(doctor::DoctorArgs),

    /// Run the red-team eval suite against the configured
    /// guardrail mode. `relix eval guardrails --mode strict`
    /// runs the full corpus; `--quick` runs a fast subset
    /// for CI smoke. Exits non-zero when the attack-block or
    /// safe-pass rates fall below the spec floor.
    Eval {
        #[command(subcommand)]
        cmd: eval::Cmd,
    },
    /// GAP 13: provenance registry inspector. `relix
    /// provenance show <trace>` prints the snapshot; `…
    /// diff <a> <b>` prints the change list; `… history
    /// [--prompt FILE]` lists prompt-file load events;
    /// `… audit [--from] [--to]` lists every snapshot in a
    /// time range.
    Provenance {
        #[command(subcommand)]
        cmd: provenance::Cmd,
    },
    /// GAP 24: two-sink session debugger. `relix sessions
    /// list` lists sessions (optionally filtered by
    /// `--agent` / `--status`); `… show <id>` prints the
    /// timeline (`--full` with a bearer via `--bearer-file`
    /// also pulls each event's content body); `… search
    /// --query Q` substring-matches session_id + agent_id.
    Sessions {
        #[command(subcommand)]
        cmd: sessions::Cmd,
    },
    /// GAP 16: provider + model inventory. `relix models
    /// list` shows every provider configured on the bridge
    /// with its default model + enabled flag; `relix models
    /// health` adds the aggregate cooldown / quarantine /
    /// rate-limit counters.
    Models {
        #[command(subcommand)]
        cmd: models::Cmd,
    },
    /// GAP 11 + 12: transactional gateway + evidence inspector.
    /// `relix execution rollback <transaction_id>` undoes a
    /// transaction; `relix execution transaction <id>` prints
    /// the full action history; `relix execution evidence
    /// [--action <id>] [--actor <name>]` lists evidence
    /// records.
    Execution {
        #[command(subcommand)]
        cmd: execution::Cmd,
    },
    /// PH-TERM-CLI: inspect + control tool.terminal.* on a
    /// tool node. `terminal sessions` lists live runs;
    /// `terminal audit` snapshots the completion ring;
    /// `terminal cancel --session-id X` triggers cooperative
    /// cancel. Libp2p dial-and-call.
    Terminal {
        #[command(subcommand)]
        cmd: terminal::Cmd,
    },
    /// GAP 10 PART 3: tool-node caps that don't fit the existing
    /// per-cap subcommands. `tool screen [--region "x,y,w,h"]
    /// [--out <file.png>]` captures the host's screen via
    /// `POST /v1/tools/screen` onto `tool.screen`.
    Tool {
        #[command(subcommand)]
        cmd: tool::Cmd,
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

    /// RELIX-7.24 Stage-1/3/4/5: Build Mode entry point.
    /// `relix build "<spec>"` runs the full planning pipeline
    /// (orchestrator + critic + conflict resolver +
    /// optional approval gate + optional step-level
    /// verification) and pretty-prints each stage. When the
    /// coordinator requires approval, the command shows the
    /// generated plan and asks for an interactive
    /// approve/reject on stdin. `--no-approval` and
    /// `--dry-run` opt out; `--output json` dumps raw
    /// responses for scripting.
    Build(build::BuildArgs),

    /// Stop the local mesh by terminating only the PIDs `relix boot`
    /// recorded in its pidfile. An unrelated mesh on the same machine
    /// survives. Idempotent - exits 0 if nothing was running.
    Stop(mesh::StopArgs),

    /// Print bridge health + topology snapshot. Exits 1 if the bridge
    /// is unreachable, so this is safe to use as a CI / shell gate.
    Status(mesh::StatusArgs),

    /// Dashboard diagnostics + recovery. `relix dashboard doctor` runs a
    /// read-only health/auth + product-loop check (is the bridge up, an
    /// admin configured, the SPA served, and the spine/prime routes wired?).
    /// `relix dashboard reset-admin` is LOCAL forgotten-password recovery.
    Dashboard {
        #[command(subcommand)]
        cmd: dashboard::Cmd,
    },

    /// Check for a newer Relix release. Hits the GitHub release API,
    /// compares against the running binary's version, and offers to
    /// download + replace if a newer version exists.
    Update(update::UpdateArgs),

    /// First-release readiness gates. `relix release readiness` prints the
    /// local release gate; `--run-local-gate` runs it without enabling hosted
    /// workflows or spending model credits.
    Release {
        #[command(subcommand)]
        cmd: release::Cmd,
    },

    /// Dependency auto-install. `relix install` (or
    /// `relix install --check`) prints a table of every
    /// dependency Relix needs (Docker, Ollama, Qdrant) with
    /// version + status. `relix install --fix` runs the
    /// platform-appropriate installer for every missing
    /// dependency after one confirmation prompt. Continues
    /// past individual failures so a missing Docker doesn't
    /// stop Ollama from installing. The `setup` wizard also
    /// runs this check before its first config page.
    Install(install::InstallArgs),

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
        Cmd::Identity { cmd } => identity::run(cmd).await,
        Cmd::Task { cmd } => task::run(cmd).await,
        Cmd::Capability { cmd } => capability::run(cmd).await,
        Cmd::Call(args) => call::run(args).await,
        Cmd::Topology { cmd } => topology::run(cmd).await,
        Cmd::Ops { cmd } => ops::run(cmd).await,
        Cmd::Email { cmd } => email::run(cmd).await,
        Cmd::Metrics { cmd } => metrics::run(cmd).await,
        Cmd::Observe(args) => observe::run(args).await,
        Cmd::Pii { cmd } => pii::run(cmd).await,
        Cmd::Training { cmd } => training::run(cmd).await,
        Cmd::Knowledge { cmd } => knowledge::run(cmd).await,
        Cmd::Confidence { cmd } => confidence::run(cmd).await,
        Cmd::Belief { cmd } => belief::run(cmd).await,
        Cmd::Approval { cmd } => approval::run(cmd).await,
        Cmd::Credentials { cmd } => credentials::run(cmd).await,
        Cmd::Judge { cmd } => judge::run(cmd).await,
        Cmd::Reasoning { cmd } => reasoning::run(cmd).await,
        Cmd::Routing { cmd } => routing::run(cmd).await,
        Cmd::Planning { cmd } => planning::run(cmd).await,
        Cmd::Router { cmd } => router::run(cmd).await,
        Cmd::Mcp { cmd } => mcp::run(cmd).await,
        Cmd::Fs { cmd } => fs::run(cmd).await,
        Cmd::Web { cmd } => web::run(cmd).await,
        Cmd::Browser { cmd } => browser::run(cmd).await,
        Cmd::Sol { cmd } => sol::run(cmd).await,
        Cmd::Flow { cmd } => {
            flow::run(cmd);
            Ok(())
        }
        Cmd::Workflow { cmd } => workflow::run(cmd).await,
        Cmd::Doctor(args) => doctor::run(args).await,
        Cmd::Eval { cmd } => eval::run(cmd).await,
        Cmd::Execution { cmd } => execution::run(cmd).await,
        Cmd::Provenance { cmd } => provenance::run(cmd).await,
        Cmd::Sessions { cmd } => sessions::run(cmd).await,
        Cmd::Models { cmd } => models::run(cmd).await,
        Cmd::Terminal { cmd } => terminal::run(cmd).await,
        Cmd::Tool { cmd } => tool::run(cmd).await,
        Cmd::Ping {
            peer,
            identity,
            method,
            client_key,
        } => ping::run(&peer, &identity, &method, &client_key).await,
        Cmd::Setup => setup::run().await,
        Cmd::Boot(args) => mesh::boot(args).await,
        Cmd::Build(args) => build::run(args).await,
        Cmd::Stop(args) => mesh::stop(args),
        Cmd::Status(args) => mesh::status(args).await,
        Cmd::Dashboard { cmd } => dashboard::run(cmd).await,
        Cmd::Update(args) => update::run(args).await,
        Cmd::Release { cmd } => release::run(cmd).await,
        Cmd::Install(args) => install::run(args).await,
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
