//! `relix-cli ops` — operator-facing CLI snapshots.
//!
//! Subcommands:
//! - `capabilities` (PH-DASH3-CLI) — hits `/v1/topology` and
//!   pretty-prints every capability the bridge has discovered,
//!   mirroring the dashboard's PH-DASH3 explorer for terminal
//!   operators.
//! - `stuck`, `dispatch-stats`, `smoke`, `policy-simulate`,
//!   `policy-denials`, `events`, `cron`, ... — see each variant
//!   for its source endpoint.
//!
//! PART 6 (this commit) removed the `providers-health` and
//! `route-test` subcommands. Both dispatched against bridge
//! endpoints (`/v1/providers/health` and
//! `/v1/providers/route_test`) that were deleted in the prior
//! security session — provider key handling no longer lives on
//! the bridge. Aggregate provider counters flow through
//! `/v1/observability/*` and `/v1/metrics/*` now.
//!
//! All subcommands are one-shot HTTP-against-bridge — useful
//! for status-line scripts, on-call triage, and tmux dashboards.

use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::json;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List every capability the bridge has discovered across
    /// every peer in the cached topology. Mirrors the dashboard's
    /// PH-DASH3 capability explorer for terminal operators.
    /// Source is `/v1/topology` — each peer's methods[]
    /// aggregated.
    Capabilities {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Filter by capability prefix (e.g. `tool.web`).
        /// Substring match, case-insensitive. Empty = all.
        #[arg(long, default_value = "")]
        filter: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// H6 stuck-running task projection from /v1/tasks/stuck.
    /// Shows tasks that have been `running` longer than
    /// `--threshold-secs` (default 300) without a deadline.
    Stuck {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Stuck threshold in seconds (passed to the bridge).
        #[arg(long, default_value_t = 300i64)]
        threshold_secs: i64,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-006c mirror: per-capability invocation + latency
    /// counters from a peer's DispatchBridge, fetched via
    /// `GET /v1/dispatch/stats`. Sorted by mean latency desc —
    /// the slowest capability shows first. Lifetime counters,
    /// reset on peer restart.
    DispatchStats {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Target peer alias.
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-008b mirror: end-to-end mesh smoke test against
    /// an already-running bridge. Hits liveness, topology,
    /// chat completion, dispatch stats, and policy denials
    /// in sequence. Exit 1 on any failure. Pure Rust port
    /// of `scripts/demo-smoke.sh` so Windows operators
    /// don't need bash.
    Smoke {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Chat model used for the round-trip step. Defaults
        /// to `relix-mock` which works without an API key
        /// regardless of provider configuration.
        #[arg(long, default_value = "relix-mock")]
        provider: String,
        /// Bridge bearer token for the auth-gated `/v1/*` steps.
        /// Precedence when omitted: `RELIX_BRIDGE_TOKEN` env, then
        /// `~/.relix/bridge-token`. Without it an auth-enabled
        /// bridge answers 401 on steps 2-5 and smoke reports a
        /// healthy mesh as broken.
        #[arg(long)]
        token: Option<String>,
    },
    /// W2-007b mirror: ask a peer's PolicyEngine "would this
    /// caller (with these groups) calling this method be
    /// allowed?" without invoking the method. Hits
    /// `GET /v1/policy/simulate`.
    PolicySimulate {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Target peer alias.
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Method to simulate (e.g. `tool.web_fetch`).
        #[arg(long)]
        method: String,
        /// Comma-separated groups list (e.g.
        /// `chat-users,operators`). Empty = inherit caller.
        #[arg(long, default_value = "")]
        groups: String,
        /// Raw JSON instead of the pretty summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-007e mirror: recent policy-denied attempts ring
    /// (capacity 256, peer-restart resets). Hits
    /// `GET /v1/policy/denials`.
    PolicyDenials {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Target peer alias.
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Maximum entries (default 100, server caps at 500).
        #[arg(long, default_value_t = 100usize)]
        max: usize,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W7-SEARCH — full-text search across chat-turn chronicle
    /// events via the bridge's `/v1/memory/sessions/search`.
    /// Operator-facing surface; agents call the same endpoint
    /// indirectly via the tool node's `memory.session_search`
    /// capability.
    SessionSearch {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Query string. Required.
        #[arg(long)]
        query: String,
        /// Optional subject_id filter. Empty (the default)
        /// searches across every session in the chronicle.
        #[arg(long, default_value = "")]
        subject_id: String,
        /// Maximum hits to return. Server caps at 100.
        #[arg(long, default_value_t = 20usize)]
        limit: usize,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-008i — one-shot snapshot of the bridge's
    /// observable state. Hits health, topology, dispatch
    /// stats, policy denials, and the recent events ring
    /// in parallel and combines them into a single JSON
    /// dump. Useful for incident attachments and offline
    /// triage — engineers without mesh access can answer
    /// "what did the mesh look like at $time" from the
    /// file alone.
    Snapshot {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Target peer alias for per-peer endpoints
        /// (dispatch stats + policy denials).
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Write to a file instead of stdout. `-` is the
        /// stdout sentinel (default). Existing files are
        /// overwritten without prompt.
        #[arg(long, default_value = "-")]
        output: String,
        /// Pretty-print the JSON (indented, easy to diff).
        #[arg(long, default_value_t = false)]
        pretty: bool,
    },
    /// W2-MEMORY-4 — read persistent agent + user memory for
    /// a subject_id. Hits the bridge's `/v1/memory/agent`.
    /// Pure read; writes happen via the agent's `memory` tool
    /// inside an ai.chat session, never via this command.
    AgentMemory {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Target peer alias (defaults to `memory`).
        #[arg(long, default_value = "memory")]
        peer: String,
        /// The agent's 64-char hex subject_id.
        #[arg(long)]
        subject_id: String,
        /// Raw JSON instead of the pretty summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-008h — print a copy-paste Open WebUI connection
    /// setup for the current bridge. Hits `/v1/models` and
    /// formats the host:port + advertised model ids into
    /// a block operators can paste into Open WebUI's
    /// Settings → Connections → OpenAI API.
    OpenWebuiSetup {
        /// Bridge HTTP base URL (used to fetch the model list).
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Hostname Open WebUI should dial. Defaults to
        /// `host.docker.internal` (the Docker-on-Mac/Windows
        /// loopback alias). Use `127.0.0.1` when Open WebUI
        /// is native, or your machine's LAN IP when remote.
        #[arg(long, default_value = "host.docker.internal")]
        host: String,
        /// Raw JSON of the bridge's `/v1/models` response.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-008d — live tail of the task firehose. Polls
    /// `/v1/tasks/events/recent?since=<cursor>` on a loop
    /// and prints each new event one-per-line. Ctrl-C
    /// exits cleanly. Lighter than SSE — pure HTTP polling,
    /// works through every proxy / shell / tmux.
    Tail {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Filter by event_type substring (case-insensitive).
        /// Empty = all.
        #[arg(long, default_value = "")]
        filter: String,
        /// Poll interval in milliseconds (default 1000).
        /// Clamped to [200, 60000].
        #[arg(long, default_value_t = 1000u64)]
        interval_ms: u64,
        /// Stop after N total events have been printed
        /// (handy for CI smoke). 0 = no limit.
        #[arg(long, default_value_t = 0usize)]
        max_events: usize,
    },
    /// Recent cross-task events from /v1/tasks/events/recent.
    /// Mirrors the dashboard firehose for terminal operators
    /// — shows the H2 one-line summary projection per row.
    Events {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Page limit (server caps at 500).
        #[arg(long, default_value_t = 50usize)]
        limit: usize,
        /// Filter by event_type substring (e.g.
        /// `task.retry`). Empty = all.
        #[arg(long, default_value = "")]
        filter: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// W2-008f: CSV output instead of the table — easy
        /// spreadsheet import. Columns:
        /// `event_id,task_id,event_type,ts,summary,payload`.
        /// Quoting matches RFC 4180.
        #[arg(long, default_value_t = false)]
        csv: bool,
    },
    /// PH-CRON-CLI: cron scheduler. Six subcommands that proxy
    /// onto the bridge's `/v1/cron/jobs` endpoints, themselves
    /// forwarding to the coordinator's `cron.*` capabilities.
    Cron(CronArgs),
    /// PH-DELEGATE-CLI: delegation surface. Four subcommands
    /// that proxy onto the bridge's `/v1/delegate/*` endpoints,
    /// themselves forwarding to the coordinator's `delegate.*`
    /// capabilities.
    Delegate(DelegateArgs),
    /// PH-AGENT-CLI: agent employee permission model.
    Agent(AgentArgs),
    /// PH-MSG-CLI: agent-to-agent messaging. Five subcommands
    /// that proxy onto the bridge's `/v1/messages` endpoints.
    Msg(MsgArgs),
    /// Discord channel surface — read-only status + recent
    /// inbound messages from the discord controller's ring.
    /// Proxies onto the bridge's `/v1/discord/*` endpoints.
    Discord(DiscordArgs),
    /// Slack channel surface — read-only status + recent
    /// inbound messages from the slack controller's ring.
    /// Proxies onto the bridge's `/v1/slack/*` endpoints.
    Slack(SlackArgs),
    /// Memory vector-embedding surface — embed text into a
    /// subject's per-target vector store, run a semantic search,
    /// or re-embed all existing entries.
    Memory(MemoryArgs),
    /// Plugin host management — list / status / reload / disable
    /// the plugins loaded by a plugin_host node.
    Plugin(PluginArgs),
}

#[derive(Args, Debug)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub cmd: PluginCmd,
}

#[derive(Subcommand, Debug)]
pub enum PluginCmd {
    List {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Status {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long = "plugin-id")]
        plugin_id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Reload {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long = "plugin-id")]
        plugin_id: String,
    },
    Disable {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long = "plugin-id")]
        plugin_id: String,
    },
}

#[derive(Args, Debug)]
pub struct MemoryArgs {
    #[command(subcommand)]
    pub cmd: MemoryCmd,
}

#[derive(Subcommand, Debug)]
pub enum MemoryCmd {
    /// Embed and store one chunk of text. Hits POST /v1/memory/embed.
    Embed {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long = "subject-id")]
        subject_id: String,
        #[arg(long, value_parser = ["agent", "user"])]
        target: String,
        #[arg(long)]
        text: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Semantic search over a subject's embeddings. Hits POST
    /// /v1/memory/search.
    Search {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long = "subject-id")]
        subject_id: String,
        #[arg(long, value_parser = ["agent", "user"])]
        target: String,
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Re-embed all existing memory entries for a subject. Hits
    /// POST /v1/memory/embed_all.
    EmbedAll {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long = "subject-id")]
        subject_id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 5: Q&A across one subject's Layer 3/4 memory. Hits
    /// POST /v1/memory/dialectic.
    Dialectic {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        observer: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        question: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 5: chunk + embed a document into Layer 2. Hits POST
    /// /v1/memory/ingest. `--content` or `--content-file` must
    /// be supplied; for binary inputs use `--content-base64-file`.
    Ingest {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        observer: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        source: String,
        #[arg(long = "content-type", default_value = "text")]
        content_type: String,
        #[arg(long)]
        content: Option<String>,
        #[arg(long = "content-file")]
        content_file: Option<String>,
        #[arg(long = "content-base64-file")]
        content_base64_file: Option<String>,
        #[arg(long = "chunk-size-chars")]
        chunk_size_chars: Option<usize>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 5: vision-embed an image into Layer 2. Hits POST
    /// /v1/memory/ingest_image. `--image-file` is read, base64-
    /// encoded, and posted to the bridge.
    IngestImage {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        observer: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        source: String,
        #[arg(long = "image-file")]
        image_file: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 5: promote in-context turns to Layer 2. Hits POST
    /// /v1/memory/context_flush.
    Flush {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long = "session-id")]
        session_id: String,
        #[arg(long = "agent")]
        agent_name: String,
        #[arg(long = "keep-recent", default_value_t = 5)]
        keep_recent_n: usize,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 6: list observation candidates parked in the quarantine
    /// table by the anomaly scorer. Hits POST
    /// /v1/memory/quarantine/list.
    QuarantineList {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 6: approve a quarantined candidate. Hits POST
    /// /v1/memory/quarantine/approve.
    QuarantineApprove {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 6: reject a quarantined candidate. Hits POST
    /// /v1/memory/quarantine/reject.
    QuarantineReject {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 7: edit one memory record's text. Hits POST
    /// /v1/memory/records/edit.
    EditRecord {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        text: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 7: freeze a memory record. Hits POST
    /// /v1/memory/records/freeze.
    FreezeRecord {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 7: unfreeze a memory record. Hits POST
    /// /v1/memory/records/unfreeze.
    UnfreezeRecord {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 7: bulk-export every record for one source. Hits
    /// POST /v1/memory/export.
    Export {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        layer: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// GAP 7: force the next promoter tick to regenerate the
    /// Layer-4 model for one source. Hits POST
    /// /v1/memory/refresh_model.
    RefreshModel {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        source: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct SlackArgs {
    #[command(subcommand)]
    pub cmd: SlackCmd,
}

#[derive(Subcommand, Debug)]
pub enum SlackCmd {
    Status {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Messages {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct DiscordArgs {
    #[command(subcommand)]
    pub cmd: DiscordCmd,
}

#[derive(Subcommand, Debug)]
pub enum DiscordCmd {
    /// Print the discord bot's live status. Hits
    /// `/v1/discord/status` and pretty-prints the JSON.
    Status {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the last N inbound messages from the discord
    /// controller's ring.
    Messages {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct MsgArgs {
    #[command(subcommand)]
    pub cmd: MsgCmd,
}

#[derive(Subcommand, Debug)]
pub enum MsgCmd {
    /// Send a message from one agent to another.
    Send {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "")]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long, default_value = "")]
        thread_id: String,
        #[arg(long, default_value = "")]
        reply_to_message_id: String,
        #[arg(long, default_value_t = 0i64)]
        ttl_secs: i64,
    },
    /// Read an agent's inbox.
    Inbox {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        subject_id: String,
        #[arg(long, default_value_t = false)]
        include_read: bool,
        #[arg(long, default_value_t = 20usize)]
        limit: usize,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Mark a message as read.
    Read {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        message_id: String,
        #[arg(long)]
        reader_subject_id: String,
    },
    /// List every message in a thread.
    Thread {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        thread_id: String,
        #[arg(long)]
        subject_id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Soft delete a message (flips status to expired).
    Delete {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        message_id: String,
        #[arg(long)]
        subject_id: String,
    },
}

#[derive(Args, Debug)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub cmd: AgentCmd,
}

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    List {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value = "")]
        subject_id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    Create {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        department: String,
        #[arg(long)]
        team: String,
        #[arg(long)]
        created_by: String,
        #[arg(long)]
        subject_id: String,
        #[arg(long, default_value = "medium")]
        risk_ceiling: String,
    },
    Get {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        agent_id: String,
    },
    Enable {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        agent_id: String,
    },
    Suspend {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        agent_id: String,
    },
    Disable {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        agent_id: String,
    },
    ApprovalsPending {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    ApprovalDecide {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        approval_id: String,
        #[arg(long, value_parser = ["approved", "rejected"])]
        decision: String,
        #[arg(long, default_value = "")]
        note: String,
        #[arg(long, default_value = "operator")]
        decided_by: String,
    },
    StandingApprovalGrant {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        agent_id: String,
        #[arg(long)]
        category: String,
        /// Duration like `30m`, `2h`, `1d`, `7d`. Computed
        /// against the current time.
        #[arg(long)]
        expires_in: String,
        #[arg(long, default_value = "")]
        note: String,
        #[arg(long, default_value = "")]
        path_glob: String,
    },
    StandingApprovalList {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        agent_id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    StandingApprovalRevoke {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        standing_id: String,
    },
}

#[derive(Args, Debug)]
pub struct DelegateArgs {
    #[command(subcommand)]
    pub cmd: DelegateCmd,
}

#[derive(Subcommand, Debug)]
pub enum DelegateCmd {
    /// Spawn a delegated child task from a parent.
    Spawn {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        parent_task_id: String,
        #[arg(long)]
        goal: String,
        #[arg(long, default_value = "")]
        context: String,
        #[arg(long, default_value = "")]
        target_subject_id: String,
        #[arg(long, default_value_t = 0usize)]
        depth: usize,
    },
    /// Read a delegated child task's status + result preview.
    Result {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        child_task_id: String,
    },
    /// Cancel a delegated child task.
    Cancel {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        child_task_id: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// List delegated children of a parent task.
    List {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        parent_task_id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct CronArgs {
    #[command(subcommand)]
    pub cmd: CronCmd,
}

#[derive(Subcommand, Debug)]
pub enum CronCmd {
    /// List cron jobs. Filter by `--subject-id` to see only
    /// one owner's jobs; omit to see all.
    List {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value = "")]
        subject_id: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Create a new cron job.
    Create {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        schedule: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        subject_id: String,
        #[arg(long, default_value = "flows/chat_template.sol")]
        flow_template: String,
    },
    /// Manually trigger a job. Creates a coordinator task
    /// immediately and dispatches `ai.chat` in the background.
    Trigger {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        job_id: String,
    },
    /// Delete a cron job permanently.
    Delete {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        job_id: String,
    },
    /// Re-enable a previously disabled job.
    Enable {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        job_id: String,
    },
    /// Disable a job without deleting it.
    Disable {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long)]
        job_id: String,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Capabilities {
            bridge,
            filter,
            json,
        } => capabilities(&bridge, &filter, json).await,
        Cmd::Stuck {
            bridge,
            threshold_secs,
            json,
        } => stuck(&bridge, threshold_secs, json).await,
        Cmd::Events {
            bridge,
            limit,
            filter,
            json,
            csv,
        } => events(&bridge, limit, &filter, json, csv).await,
        Cmd::DispatchStats { bridge, peer, json } => dispatch_stats(&bridge, &peer, json).await,
        Cmd::PolicySimulate {
            bridge,
            peer,
            method,
            groups,
            json,
        } => policy_simulate(&bridge, &peer, &method, &groups, json).await,
        Cmd::PolicyDenials {
            bridge,
            peer,
            max,
            json,
        } => policy_denials(&bridge, &peer, max, json).await,
        Cmd::SessionSearch {
            bridge,
            query,
            subject_id,
            limit,
            json,
        } => session_search(&bridge, &query, &subject_id, limit, json).await,
        Cmd::Smoke {
            bridge,
            provider,
            token,
        } => smoke(&bridge, &provider, token.as_deref()).await,
        Cmd::Tail {
            bridge,
            filter,
            interval_ms,
            max_events,
        } => tail(&bridge, &filter, interval_ms, max_events).await,
        Cmd::OpenWebuiSetup { bridge, host, json } => openwebui_setup(&bridge, &host, json).await,
        Cmd::AgentMemory {
            bridge,
            peer,
            subject_id,
            json,
        } => agent_memory(&bridge, &peer, &subject_id, json).await,
        Cmd::Snapshot {
            bridge,
            peer,
            output,
            pretty,
        } => snapshot(&bridge, &peer, &output, pretty).await,
        Cmd::Cron(args) => cron_run(args.cmd).await,
        Cmd::Delegate(args) => delegate_run(args.cmd).await,
        Cmd::Agent(args) => agent_run(args.cmd).await,
        Cmd::Msg(args) => msg_run(args.cmd).await,
        Cmd::Discord(args) => discord_run(args.cmd).await,
        Cmd::Slack(args) => slack_run(args.cmd).await,
        Cmd::Memory(args) => memory_run(args.cmd).await,
        Cmd::Plugin(args) => plugin_run(args.cmd).await,
    }
}

async fn memory_run(cmd: MemoryCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        MemoryCmd::Embed {
            bridge,
            subject_id,
            target,
            text,
            json,
        } => memory_embed_cmd(&bridge, &subject_id, &target, &text, json).await,
        MemoryCmd::Search {
            bridge,
            subject_id,
            target,
            query,
            limit,
            json,
        } => memory_search_cmd(&bridge, &subject_id, &target, &query, limit, json).await,
        MemoryCmd::EmbedAll {
            bridge,
            subject_id,
            json,
        } => memory_embed_all_cmd(&bridge, &subject_id, json).await,
        MemoryCmd::Dialectic {
            bridge,
            observer,
            subject,
            question,
            json,
        } => memory_dialectic_cmd(&bridge, &observer, &subject, &question, json).await,
        MemoryCmd::Ingest {
            bridge,
            observer,
            subject,
            source,
            content_type,
            content,
            content_file,
            content_base64_file,
            chunk_size_chars,
            json,
        } => {
            memory_ingest_cmd(
                &bridge,
                &observer,
                &subject,
                &source,
                &content_type,
                content.as_deref(),
                content_file.as_deref(),
                content_base64_file.as_deref(),
                chunk_size_chars,
                json,
            )
            .await
        }
        MemoryCmd::IngestImage {
            bridge,
            observer,
            subject,
            source,
            image_file,
            json,
        } => {
            memory_ingest_image_cmd(&bridge, &observer, &subject, &source, &image_file, json).await
        }
        MemoryCmd::Flush {
            bridge,
            session_id,
            agent_name,
            keep_recent_n,
            json,
        } => memory_context_flush_cmd(&bridge, &session_id, &agent_name, keep_recent_n, json).await,
        MemoryCmd::QuarantineList {
            bridge,
            limit,
            source,
            json,
        } => memory_quarantine_list_cmd(&bridge, limit, source.as_deref(), json).await,
        MemoryCmd::QuarantineApprove { bridge, id, json } => {
            memory_quarantine_approve_cmd(&bridge, &id, json).await
        }
        MemoryCmd::QuarantineReject { bridge, id, json } => {
            memory_quarantine_reject_cmd(&bridge, &id, json).await
        }
        MemoryCmd::EditRecord {
            bridge,
            id,
            text,
            json,
        } => memory_edit_record_cmd(&bridge, &id, &text, json).await,
        MemoryCmd::FreezeRecord { bridge, id, json } => {
            memory_freeze_record_cmd(&bridge, &id, true, json).await
        }
        MemoryCmd::UnfreezeRecord { bridge, id, json } => {
            memory_freeze_record_cmd(&bridge, &id, false, json).await
        }
        MemoryCmd::Export {
            bridge,
            source,
            layer,
            json,
        } => memory_bulk_export_cmd(&bridge, &source, layer.as_deref(), json).await,
        MemoryCmd::RefreshModel {
            bridge,
            source,
            json,
        } => memory_refresh_model_cmd(&bridge, &source, json).await,
    }
}

async fn memory_edit_record_cmd(
    bridge: &str,
    id: &str,
    text: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({ "id": id, "text": text });
    post_json_to_bridge(bridge, "/v1/memory/records/edit", &body, json_out).await
}

async fn memory_freeze_record_cmd(
    bridge: &str,
    id: &str,
    freeze: bool,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({ "id": id });
    let path = if freeze {
        "/v1/memory/records/freeze"
    } else {
        "/v1/memory/records/unfreeze"
    };
    post_json_to_bridge(bridge, path, &body, json_out).await
}

async fn memory_bulk_export_cmd(
    bridge: &str,
    source: &str,
    layer: Option<&str>,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut body = serde_json::json!({ "source": source });
    if let Some(l) = layer {
        body.as_object_mut()
            .unwrap()
            .insert("layer".into(), serde_json::Value::from(l));
    }
    post_json_to_bridge(bridge, "/v1/memory/export", &body, json_out).await
}

async fn memory_refresh_model_cmd(
    bridge: &str,
    source: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({ "source": source });
    post_json_to_bridge(bridge, "/v1/memory/refresh_model", &body, json_out).await
}

async fn post_json_to_bridge(
    bridge: &str,
    path: &str,
    body: &serde_json::Value,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}{path}");
    let client = reqwest::Client::new();
    let r = client.post(&url).json(body).send().await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn memory_quarantine_list_cmd(
    bridge: &str,
    limit: usize,
    source: Option<&str>,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut body = serde_json::json!({ "limit": limit });
    if let Some(s) = source {
        body.as_object_mut()
            .unwrap()
            .insert("source".into(), serde_json::Value::from(s));
    }
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/quarantine/list");
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn memory_quarantine_approve_cmd(
    bridge: &str,
    id: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({ "id": id });
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/quarantine/approve");
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn memory_quarantine_reject_cmd(
    bridge: &str,
    id: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({ "id": id });
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/quarantine/reject");
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn memory_dialectic_cmd(
    bridge: &str,
    observer: &str,
    subject: &str,
    question: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/dialectic");
    let client = reqwest::Client::new();
    let r = client
        .post(&url)
        .json(&json!({
            "observer_id": observer,
            "subject_id": subject,
            "question": question,
        }))
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let answer = v.get("answer").and_then(|x| x.as_str()).unwrap_or("");
    let confidence = v.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let sources = v.get("sources_used").and_then(|x| x.as_u64()).unwrap_or(0);
    let model = v.get("model_used").and_then(|x| x.as_str()).unwrap_or("");
    let fallback = v.get("fallback_reason").and_then(|x| x.as_str());
    println!("model       {model}");
    println!("sources     {sources}");
    println!("confidence  {confidence:.2}");
    if let Some(reason) = fallback {
        println!("fallback    {reason}");
    }
    println!();
    println!("{answer}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn memory_ingest_cmd(
    bridge: &str,
    observer: &str,
    subject: &str,
    source: &str,
    content_type: &str,
    content: Option<&str>,
    content_file: Option<&str>,
    content_base64_file: Option<&str>,
    chunk_size_chars: Option<usize>,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine;
    let mut body = serde_json::json!({
        "observer_id": observer,
        "subject_id": subject,
        "source": source,
        "content_type": content_type,
    });
    if let Some(n) = chunk_size_chars {
        body.as_object_mut()
            .unwrap()
            .insert("chunk_size_chars".into(), serde_json::Value::from(n));
    }
    let provided = [
        content.is_some(),
        content_file.is_some(),
        content_base64_file.is_some(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if provided != 1 {
        return Err(
            "exactly one of --content / --content-file / --content-base64-file is required".into(),
        );
    }
    if let Some(s) = content {
        body.as_object_mut()
            .unwrap()
            .insert("content".into(), serde_json::Value::from(s));
    } else if let Some(path) = content_file {
        let s = std::fs::read_to_string(path)?;
        body.as_object_mut()
            .unwrap()
            .insert("content".into(), serde_json::Value::from(s));
    } else if let Some(path) = content_base64_file {
        let bytes = std::fs::read(path)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        body.as_object_mut()
            .unwrap()
            .insert("content_base64".into(), serde_json::Value::from(encoded));
    }
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/ingest");
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn memory_ingest_image_cmd(
    bridge: &str,
    observer: &str,
    subject: &str,
    source: &str,
    image_file: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine;
    let bytes = std::fs::read(image_file)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let body = serde_json::json!({
        "observer_id": observer,
        "subject_id": subject,
        "source": source,
        "image_data": encoded,
    });
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/ingest_image");
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn memory_context_flush_cmd(
    bridge: &str,
    session_id: &str,
    agent_name: &str,
    keep_recent_n: usize,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::json!({
        "session_id": session_id,
        "agent_name": agent_name,
        "keep_recent_n": keep_recent_n,
    });
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/context_flush");
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn memory_embed_cmd(
    bridge: &str,
    subject_id: &str,
    target: &str,
    text: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/embed");
    let client = reqwest::Client::new();
    let r = client
        .post(&url)
        .json(&json!({
            "subject_id": subject_id,
            "target": target,
            "text": text,
        }))
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let id = v.get("embedding_id").and_then(|x| x.as_str()).unwrap_or("");
    let already = v
        .get("already_present")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    println!(
        "embedding_id      {id}{}",
        if already { "  (already present)" } else { "" }
    );
    Ok(())
}

async fn memory_search_cmd(
    bridge: &str,
    subject_id: &str,
    target: &str,
    query: &str,
    limit: usize,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/search");
    let client = reqwest::Client::new();
    let r = client
        .post(&url)
        .json(&json!({
            "subject_id": subject_id,
            "target": target,
            "query": query,
            "limit": limit,
        }))
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let results = v
        .get("results")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        println!("(no results)");
        return Ok(());
    }
    println!("{:<18} {:<8} chunk_text", "embedding_id", "score");
    for hit in results {
        let id = hit
            .get("embedding_id")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let score = hit.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let chunk = hit.get("chunk_text").and_then(|x| x.as_str()).unwrap_or("");
        println!("{:<18} {:<8.4} {}", short(id, 18), score as f32, chunk);
    }
    Ok(())
}

async fn memory_embed_all_cmd(
    bridge: &str,
    subject_id: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/embed_all");
    let client = reqwest::Client::new();
    let r = client
        .post(&url)
        .json(&json!({ "subject_id": subject_id }))
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let n = v
        .get("chunks_embedded")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
    println!(
        "ok                {}\nchunks_embedded   {n}",
        if ok { "yes" } else { "no" }
    );
    Ok(())
}

async fn slack_run(cmd: SlackCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SlackCmd::Status { bridge, json } => slack_status(&bridge, json).await,
        SlackCmd::Messages {
            bridge,
            limit,
            json,
        } => slack_messages(&bridge, limit, json).await,
    }
}

async fn slack_status(bridge: &str, json_out: bool) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let body = http_get_string(&format!("{base}/v1/slack/status")).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let online = v.get("online").and_then(|x| x.as_bool()).unwrap_or(false);
    let last = v.get("last_message_at").and_then(|x| x.as_i64());
    let last_disp = match last {
        Some(t) => format!("{t}"),
        None => "—".to_string(),
    };
    println!("online            {}", if online { "yes" } else { "no" });
    println!(
        "username          @{}",
        v.get("username").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "user_id           {}",
        v.get("user_id").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "team_id           {}",
        v.get("team_id").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "channel_id        {}",
        v.get("channel_id").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "messages_seen     {}",
        v.get("messages_seen").and_then(|x| x.as_u64()).unwrap_or(0)
    );
    println!("last_message_at   {last_disp}");
    Ok(())
}

async fn slack_messages(
    bridge: &str,
    limit: usize,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let body = http_get_string(&format!("{base}/v1/slack/messages/recent?limit={limit}")).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let rows = v
        .get("messages")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no messages)");
        return Ok(());
    }
    println!("{:<22} {:<14} {:<16} content", "ts", "user_id", "username");
    for m in rows {
        let ts = m.get("ts").and_then(|x| x.as_str()).unwrap_or("");
        let user_id = m.get("user_id").and_then(|x| x.as_str()).unwrap_or("");
        let username = m.get("username").and_then(|x| x.as_str()).unwrap_or("");
        let content = m
            .get("content_preview")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        println!(
            "{:<22} {:<14} {:<16} {}",
            short(ts, 22),
            short(user_id, 14),
            short(username, 16),
            content
        );
    }
    Ok(())
}

async fn discord_run(cmd: DiscordCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        DiscordCmd::Status { bridge, json } => discord_status(&bridge, json).await,
        DiscordCmd::Messages {
            bridge,
            limit,
            json,
        } => discord_messages(&bridge, limit, json).await,
    }
}

async fn discord_status(bridge: &str, json_out: bool) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let body = http_get_string(&format!("{base}/v1/discord/status")).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let online = v.get("online").and_then(|x| x.as_bool()).unwrap_or(false);
    let last = v.get("last_message_at").and_then(|x| x.as_i64());
    let last_disp = match last {
        Some(t) => format!("{t}"),
        None => "—".to_string(),
    };
    println!("online            {}", if online { "yes" } else { "no" });
    println!(
        "username          @{}",
        v.get("username").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "user_id           {}",
        v.get("user_id").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "channel_id        {}",
        v.get("channel_id").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "messages_seen     {}",
        v.get("messages_seen").and_then(|x| x.as_u64()).unwrap_or(0)
    );
    println!("last_message_at   {last_disp}");
    Ok(())
}

async fn discord_messages(
    bridge: &str,
    limit: usize,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let body = http_get_string(&format!("{base}/v1/discord/messages/recent?limit={limit}")).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let rows = v
        .get("messages")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no messages)");
        return Ok(());
    }
    println!("{:<22} {:<20} {:<16} content", "ts", "user_id", "username");
    for m in rows {
        let ts = m.get("ts").and_then(|x| x.as_i64()).unwrap_or(0);
        let user_id = m.get("user_id").and_then(|x| x.as_str()).unwrap_or("");
        let username = m.get("username").and_then(|x| x.as_str()).unwrap_or("");
        let content = m
            .get("content_preview")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        println!(
            "{:<22} {:<20} {:<16} {}",
            ts,
            short(user_id, 20),
            short(username, 16),
            content
        );
    }
    Ok(())
}

// W2-008i CLI: combined-state snapshot for incident attachments.

async fn snapshot(
    bridge: &str,
    peer: &str,
    output: &str,
    pretty: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    // Bind the URLs first so the format!() temporaries
    // live across the tokio::join! await; passing
    // `&format!(...)` inline races their drop.
    let u_health = format!("{base}/v1/health");
    let u_topology = format!("{base}/v1/topology");
    let u_dispatch = format!("{base}/v1/dispatch/stats?peer={peer}");
    let u_denials = format!("{base}/v1/policy/denials?peer={peer}&max=100");
    let u_events = format!("{base}/v1/tasks/events/recent?limit=100");
    // Run the five fetches concurrently — each is a cheap
    // HTTP GET and incident-response wants the dump fast.
    let (health, topology, dispatch_stats, denials, events) = tokio::join!(
        http_get(&u_health),
        http_get(&u_topology),
        http_get(&u_dispatch),
        http_get(&u_denials),
        http_get(&u_events),
    );
    // Each endpoint's value is either the parsed JSON
    // payload (preferred — pretty-prints cleanly) or an
    // error string when the fetch failed. Operators see
    // partial state instead of a hard fail; one section
    // being missing in a triage dump is still useful.
    let entry = |name: &str,
                 res: Result<String, Box<dyn std::error::Error>>|
     -> (String, serde_json::Value) {
        let v = match res {
            Ok(body) => serde_json::from_str::<serde_json::Value>(&body)
                .unwrap_or(serde_json::Value::String(body)),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        (name.to_string(), v)
    };
    let mut obj = serde_json::Map::new();
    obj.insert(
        "snapshot_at".to_string(),
        serde_json::json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
    );
    obj.insert("bridge".to_string(), serde_json::json!(base));
    obj.insert("peer".to_string(), serde_json::json!(peer));
    let entries = [
        entry("health", health),
        entry("topology", topology),
        entry("dispatch_stats", dispatch_stats),
        entry("denials", denials),
        entry("events", events),
    ];
    for (k, v) in entries {
        obj.insert(k, v);
    }
    let value = serde_json::Value::Object(obj);
    let text = if pretty {
        serde_json::to_string_pretty(&value)?
    } else {
        serde_json::to_string(&value)?
    };
    if output == "-" {
        println!("{text}");
    } else {
        std::fs::write(output, &text).map_err(|e| format!("write {output}: {e}"))?;
        // Status line on stderr so `> file` redirection
        // remains clean if the operator passes `-` instead.
        eprintln!("wrote snapshot to {output} ({len} bytes)", len = text.len());
    }
    Ok(())
}

// W2-008h CLI: print Open WebUI connection setup.

#[derive(Debug, Deserialize)]
struct ModelsResp {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    description: String,
}

/// W2-008h: derive the bridge's listening port from the
/// `--bridge` URL (`http://127.0.0.1:19791` → `19791`).
/// Falls back to `19791` (the default).
fn port_from_bridge(bridge: &str) -> u16 {
    bridge
        .trim_end_matches('/')
        .rsplit_once(':')
        .and_then(|(_, p)| p.trim_end_matches('/').parse::<u16>().ok())
        .unwrap_or(19791)
}

// W2-MEMORY-4 CLI mirror: GET /v1/memory/agent

#[derive(Debug, Deserialize)]
struct AgentMemoryResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    subject_id: String,
    #[serde(default)]
    agent_memory: String,
    #[serde(default)]
    user_memory: String,
    #[serde(default)]
    agent_chars: usize,
    #[serde(default)]
    user_chars: usize,
}

async fn agent_memory(
    bridge: &str,
    peer: &str,
    subject_id: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let subj = subject_id.trim();
    if subj.is_empty() {
        return Err("--subject-id required".into());
    }
    let url = format!(
        "{}/v1/memory/agent?peer={}&subject_id={}",
        bridge.trim_end_matches('/'),
        urlencoding(peer),
        urlencoding(subj),
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let r: AgentMemoryResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/memory/agent body: {e} (body={body})"))?;
    println!("peer={p}  subject_id={s}", p = r.peer, s = r.subject_id);
    println!();
    println!("--- AGENT MEMORY ({n} / 2200 chars) ---", n = r.agent_chars);
    if r.agent_memory.is_empty() {
        println!("(empty)");
    } else {
        println!("{}", r.agent_memory);
    }
    println!();
    println!("--- USER MEMORY ({n} / 1375 chars) ---", n = r.user_chars);
    if r.user_memory.is_empty() {
        println!("(empty)");
    } else {
        println!("{}", r.user_memory);
    }
    println!();
    println!("(Entry delimiter: § U+00A7. Memory is per-subject_id —");
    println!(" each agent's identity bundle subject_id keys its own row.)");
    Ok(())
}

async fn openwebui_setup(
    bridge: &str,
    host: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/models", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let resp: ModelsResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/models body: {e} (body={body})"))?;
    let port = port_from_bridge(bridge);
    println!("Open WebUI connection setup");
    println!("Settings → Connections → OpenAI API");
    println!();
    println!("  API Base URL: http://{host}:{port}/v1");
    println!("  API Key:      relix   (any non-empty string works)");
    println!();
    if resp.data.is_empty() {
        println!("  Models:       (none advertised — bridge has no");
        println!("                [openai_compat.models] entries and no");
        println!("                ai.chat-capable peer in the manifest cache)");
    } else {
        println!("  Models:");
        for m in &resp.data {
            let desc = if m.description.is_empty() {
                String::from("(no description)")
            } else {
                m.description.clone()
            };
            println!("    {id:<24}  {desc}", id = m.id, desc = desc);
        }
    }
    println!();
    println!("Note: when running native (no docker), use --host 127.0.0.1.");
    println!("When Open WebUI is on another machine, use this host's LAN IP.");
    Ok(())
}

// W2-008d CLI live-tail: poll /v1/tasks/events/recent on a loop.

async fn tail(
    bridge: &str,
    filter: &str,
    interval_ms: u64,
    max_events: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let interval = std::time::Duration::from_millis(interval_ms.clamp(200, 60_000));
    let needle = filter.trim().to_ascii_lowercase();
    let mut since: i64 = 0;
    let mut printed: usize = 0;
    // Header so operators know what they're looking at —
    // matches the `events` subcommand columns.
    eprintln!(
        "tailing {base}/v1/tasks/events/recent  interval={ms}ms  filter='{f}'  (Ctrl-C to stop)",
        ms = interval.as_millis(),
        f = filter,
    );
    let ev_h = "event_type";
    let tid_h = "task_id";
    let id_h = "id";
    let sum_h = "summary";
    println!("{ev_h:<28}  {tid_h:<10}  {id_h:>6}  {sum_h}");
    loop {
        // Page size is intentionally small per tick — the
        // operator polling cadence is the rate limiter, not
        // the page. Bridge caps internally too.
        let url = if since > 0 {
            format!("{base}/v1/tasks/events/recent?limit=50&since={since}")
        } else {
            format!("{base}/v1/tasks/events/recent?limit=50")
        };
        match http_get(&url).await {
            Ok(body) => {
                let resp: EventsResponse = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("(decode failed: {e})");
                        tokio::time::sleep(interval).await;
                        continue;
                    }
                };
                // The bridge returns events oldest-first
                // within a since= window — print in that
                // order so operators read top-to-bottom as
                // time flows.
                for r in &resp.items {
                    if !needle.is_empty() && !r.event_type.to_ascii_lowercase().contains(&needle) {
                        continue;
                    }
                    let short = if r.task_id.len() > 8 {
                        &r.task_id[..8]
                    } else {
                        &r.task_id
                    };
                    let sum = if r.summary.is_empty() {
                        r.payload.as_str()
                    } else {
                        r.summary.as_str()
                    };
                    println!(
                        "{et:<28}  {tid:<10}  {id:>6}  {sum}",
                        et = r.event_type,
                        tid = short,
                        id = r.event_id,
                        sum = sum,
                    );
                    printed += 1;
                    if max_events > 0 && printed >= max_events {
                        eprintln!("(reached --max-events={max_events})");
                        return Ok(());
                    }
                }
                // Advance the cursor for the next tick. The
                // bridge guarantees next_cursor monotonic
                // across calls; we trust it.
                if resp.next_cursor > since {
                    since = resp.next_cursor;
                }
            }
            Err(e) => {
                // Don't bail on a transient blip — operators
                // care about tail resilience. Log once per
                // failed poll, sleep, retry.
                eprintln!("(poll failed: {e})");
            }
        }
        tokio::time::sleep(interval).await;
    }
}

// W2-008b CLI mirror: end-to-end mesh smoke test.

async fn smoke(
    bridge: &str,
    provider: &str,
    token: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    // Steps 2-5 hit auth-gated `/v1/*`. Resolve the bearer once so a
    // healthy auth-enabled mesh passes instead of reporting 401s.
    let resolved = crate::bridge_token::resolve(token);
    let bearer = resolved.as_ref().map(|(t, _)| t.as_str());
    println!("Relix smoke (bridge={base})");
    if let Some((_, src)) = &resolved {
        println!("  token: {}", src.label());
    }
    let mut step = 0usize;
    let mut fails = 0usize;
    let mut run = |desc: &str, res: Result<String, Box<dyn std::error::Error>>| {
        step += 1;
        match &res {
            Ok(_) => println!("  step {step} OK   — {desc}"),
            Err(e) => {
                println!("  step {step} FAIL — {desc}");
                eprintln!("         {e}");
                fails += 1;
            }
        }
        res.ok()
    };

    // 1. liveness — public, no token required.
    let _ = run("GET /health", http_get(&format!("{base}/health")).await);

    // 2. topology — count peers when we got a body back
    let topo_body = run(
        "GET /v1/topology",
        http_get_auth(&format!("{base}/v1/topology"), bearer).await,
    );
    if let Some(body) = topo_body {
        let peer_count = body.matches("\"alias\":").count();
        println!("         peers discovered: {peer_count}");
    }

    // 3. chat completion (mock by default)
    let chat_body = format!(
        r#"{{"model":"{provider}","messages":[{{"role":"user","content":"smoke test ping"}}]}}"#
    );
    let _ = run(
        &format!("POST /v1/chat/completions (model={provider})"),
        http_post_json_auth(&format!("{base}/v1/chat/completions"), &chat_body, bearer).await,
    );

    // 4. dispatch stats — observability
    let _ = run(
        "GET /v1/dispatch/stats?peer=tool (W2-006c)",
        http_get_auth(&format!("{base}/v1/dispatch/stats?peer=tool"), bearer).await,
    );

    // 5. policy denials — yellow-flag a non-empty ring
    let denials_body = run(
        "GET /v1/policy/denials?peer=tool (W2-007e)",
        http_get_auth(
            &format!("{base}/v1/policy/denials?peer=tool&max=10"),
            bearer,
        )
        .await,
    );
    if let Some(body) = denials_body {
        // Loose-parse just the count field — keeps the smoke
        // path zero-dependency on the full denials struct.
        let count = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("count").and_then(|c| c.as_u64()))
            .unwrap_or(0);
        if count > 0 {
            println!("         ⚠  {count} recent denial(s) on tool — investigate via:");
            println!("         relix-cli ops policy-denials --peer tool");
        } else {
            println!("         denial ring empty on tool");
        }
    }

    println!();
    if fails == 0 {
        println!("smoke PASS — {step}/{step} steps OK");
        Ok(())
    } else {
        Err(format!("smoke FAIL — {fails}/{step} step(s) failed").into())
    }
}

async fn capabilities(
    bridge: &str,
    filter: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/topology", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let topo: TopologyResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    let needle = filter.trim().to_ascii_lowercase();
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    for p in &topo.peers {
        let alias = p.alias.clone().unwrap_or_else(|| "(none)".to_string());
        for m in &p.methods {
            if !needle.is_empty() && !m.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            rows.push((
                m.clone(),
                alias.clone(),
                p.node_type.clone(),
                p.freshness.clone(),
            ));
        }
    }
    rows.sort();
    let total_methods: usize = topo.peers.iter().map(|p| p.methods.len()).sum();
    println!(
        "capabilities  shown={shown}  total={total}  peers={peers}",
        shown = rows.len(),
        total = total_methods,
        peers = topo.peers.len(),
    );
    if rows.is_empty() {
        if needle.is_empty() {
            println!("(no capabilities discovered yet)");
        } else {
            println!("(no capabilities match filter \"{needle}\")");
        }
        return Ok(());
    }
    println!();
    let m_h = "capability";
    let a_h = "alias";
    let t_h = "node_type";
    let f_h = "freshness";
    println!("{m_h:<36}  {a_h:<14}  {t_h:<14}  {f_h}");
    for (method, alias, node_type, fresh) in &rows {
        println!("{method:<36}  {alias:<14}  {node_type:<14}  {fresh}",);
    }
    Ok(())
}

async fn stuck(
    bridge: &str,
    threshold_secs: i64,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/tasks/stuck?threshold_secs={}",
        bridge.trim_end_matches('/'),
        threshold_secs.max(0),
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let s: StuckResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    println!(
        "stuck={count}  threshold_secs={threshold}",
        count = s.count,
        threshold = s.threshold_secs.unwrap_or(threshold_secs),
    );
    if s.items.is_empty() {
        println!("(no stuck tasks)");
        return Ok(());
    }
    println!();
    let id_h = "task_id";
    let title_h = "title";
    let age_h = "age";
    println!("{id_h:<36}  {title_h:<32}  {age_h}");
    for it in &s.items {
        println!(
            "{id:<36}  {title:<32}  {age}s",
            id = it.task_id,
            title = it.title,
            age = it.age_secs,
        );
    }
    Ok(())
}

async fn events(
    bridge: &str,
    limit: usize,
    filter: &str,
    json: bool,
    csv: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cap = limit.clamp(1, 500);
    let url = format!(
        "{}/v1/tasks/events/recent?limit={}",
        bridge.trim_end_matches('/'),
        cap,
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let resp: EventsResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    let needle = filter.trim().to_ascii_lowercase();
    let filtered: Vec<&EventRow> = resp
        .items
        .iter()
        .filter(|r| needle.is_empty() || r.event_type.to_ascii_lowercase().contains(&needle))
        .collect();
    // W2-008f: CSV branch — RFC 4180 quoting, no table
    // headers stderr noise so the output pipes cleanly
    // into `> events.csv`.
    if csv {
        println!("event_id,task_id,event_type,ts,summary,payload");
        for r in &filtered {
            println!(
                "{id},{tid},{et},{ts},{sum},{pl}",
                id = r.event_id,
                tid = csv_field(&r.task_id),
                et = csv_field(&r.event_type),
                ts = r.ts,
                sum = csv_field(&r.summary),
                pl = csv_field(&r.payload),
            );
        }
        return Ok(());
    }
    println!(
        "events  shown={shown}  fetched={fetched}  next_cursor={cursor}",
        shown = filtered.len(),
        fetched = resp.items.len(),
        cursor = resp.next_cursor,
    );
    if filtered.is_empty() {
        if needle.is_empty() {
            println!("(no events)");
        } else {
            println!("(no events match filter \"{needle}\")");
        }
        return Ok(());
    }
    println!();
    let ev_h = "event_type";
    let tid_h = "task_id";
    let id_h = "id";
    let sum_h = "summary";
    println!("{ev_h:<28}  {tid_h:<10}  {id_h:>6}  {sum_h}");
    for r in &filtered {
        let short = if r.task_id.len() > 8 {
            &r.task_id[..8]
        } else {
            &r.task_id
        };
        let sum = if r.summary.is_empty() {
            r.payload.as_str()
        } else {
            r.summary.as_str()
        };
        println!(
            "{et:<28}  {tid:<10}  {id:>6}  {sum}",
            et = r.event_type,
            tid = short,
            id = r.event_id,
            sum = sum,
        );
    }
    Ok(())
}

/// Auth-aware POST. Attaches `Authorization: Bearer <token>` when a
/// token is supplied and turns a 401/403 into an actionable hint that
/// names the token locations instead of a raw status dump.
async fn http_post_json_auth(
    url: &str,
    body: &str,
    bearer: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(t) = bearer {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "bridge returned HTTP {status}. {}",
            crate::bridge_token::missing_token_hint()
        )
        .into());
    }
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

#[derive(Debug, Deserialize)]
struct EventsResponse {
    #[serde(default)]
    items: Vec<EventRow>,
    #[serde(default)]
    next_cursor: i64,
}

#[derive(Debug, Deserialize)]
struct EventRow {
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    event_id: i64,
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    summary: String,
    /// W2-008f: unix-seconds timestamp the bridge ships
    /// (defaults to 0 on older bridges that don't surface it).
    #[serde(default)]
    ts: i64,
}

/// W2-008f: RFC 4180 quoting — wrap in double-quotes when
/// the value contains `,` `"` newline or CR; double any
/// embedded `"`.
fn csv_field(s: &str) -> String {
    let needs_quote = s.bytes().any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'));
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 4);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push_str("\"\"");
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Deserialize)]
struct StuckResponse {
    #[serde(default)]
    items: Vec<StuckItem>,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    threshold_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct StuckItem {
    task_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    age_secs: i64,
}

#[derive(Debug, Deserialize)]
struct TopologyResponse {
    #[serde(default)]
    peers: Vec<TopologyPeer>,
}

#[derive(Debug, Deserialize)]
struct TopologyPeer {
    #[serde(default)]
    alias: Option<String>,
    node_type: String,
    #[serde(default)]
    methods: Vec<String>,
    #[serde(default)]
    freshness: String,
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    http_get_auth(url, None).await
}

/// Auth-aware GET. Attaches `Authorization: Bearer <token>` when a
/// token is supplied and turns a 401/403 into an actionable hint that
/// names the token locations instead of a raw status dump.
async fn http_get_auth(
    url: &str,
    bearer: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let mut req = client.get(url);
    if let Some(t) = bearer {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "bridge returned HTTP {status}. {}",
            crate::bridge_token::missing_token_hint()
        )
        .into());
    }
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

// W2-006c CLI mirror: GET /v1/dispatch/stats?peer=...

#[derive(Debug, Deserialize)]
struct DispatchStatsResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    rows: Vec<DispatchStatsRow>,
    #[serde(default)]
    count: usize,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // last_invoked_at / last_error_at preserved for future "stale" detection
struct DispatchStatsRow {
    #[serde(default)]
    method: String,
    #[serde(default)]
    invocations: u64,
    #[serde(default)]
    errors: u64,
    #[serde(default)]
    denied: u64,
    #[serde(default)]
    unknown_method: u64,
    #[serde(default)]
    last_invoked_at: i64,
    #[serde(default)]
    last_error_at: Option<i64>,
    #[serde(default)]
    latency_samples: u64,
    #[serde(default)]
    last_elapsed_ms: u64,
    #[serde(default)]
    max_elapsed_ms: u64,
    #[serde(default)]
    mean_elapsed_ms: u64,
    /// W2-006d: recent per-call latencies ring (oldest-first,
    /// capped at 32 by the runtime). Empty when the responder
    /// is an older peer that doesn't ship the column.
    #[serde(default)]
    recent_latencies: Vec<u32>,
}

/// W2-006d: render a ring of latency samples as a Unicode
/// block-character sparkline. Heights normalize to the ring's
/// own max so a 5ms-mean method and a 2000ms-mean method both
/// render legibly side-by-side.
fn ascii_sparkline(samples: &[u32]) -> String {
    if samples.is_empty() {
        return "-".to_string();
    }
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = (*samples.iter().max().unwrap_or(&1)).max(1);
    samples
        .iter()
        .map(|&v| {
            // Map [0..=max] → BARS index. f64 keeps the
            // mapping stable across the full u32 range
            // without integer overflow.
            let idx = ((v as f64 / max as f64) * (BARS.len() - 1) as f64).round() as usize;
            BARS[idx.min(BARS.len() - 1)]
        })
        .collect()
}

async fn dispatch_stats(
    bridge: &str,
    peer: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/dispatch/stats?peer={peer}",
        bridge.trim_end_matches('/')
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let parsed: DispatchStatsResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/dispatch/stats body: {e} (body={body})"))?;
    if parsed.rows.is_empty() {
        println!(
            "(no dispatch activity on peer '{p}' — count={c})",
            p = parsed.peer,
            c = parsed.count
        );
        return Ok(());
    }
    // Sort by mean elapsed desc (tied: invocations desc).
    let mut rows = parsed.rows;
    rows.sort_by(|a, b| {
        b.mean_elapsed_ms
            .cmp(&a.mean_elapsed_ms)
            .then_with(|| b.invocations.cmp(&a.invocations))
    });
    let m_h = "method";
    let i_h = "invocs";
    let e_h = "errs";
    let mean_h = "mean";
    let max_h = "max";
    let last_h = "last";
    let samples_h = "samples";
    let trend_h = "trend";
    println!(
        "{m_h:<36}  {i_h:>7}  {e_h:>5}  {mean_h:>6}  {max_h:>6}  {last_h:>6}  {samples_h:>7}  {trend_h}",
    );
    for r in &rows {
        let method = truncate(&r.method, 36);
        let errs = r.errors + r.denied + r.unknown_method;
        let trend = ascii_sparkline(&r.recent_latencies);
        println!(
            "{method:<36}  {invocs:>7}  {errs:>5}  {mean:>5}ms  {max:>5}ms  {last:>5}ms  {samples:>7}  {trend}",
            method = method,
            invocs = r.invocations,
            errs = errs,
            mean = r.mean_elapsed_ms,
            max = r.max_elapsed_ms,
            last = r.last_elapsed_ms,
            samples = r.latency_samples,
            trend = trend,
        );
    }
    println!("count={}", parsed.count);
    Ok(())
}

// W2-007b CLI mirror: GET /v1/policy/simulate

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PolicySimulateResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    matched_rule: Option<String>,
    #[serde(default)]
    reason: String,
}

async fn policy_simulate(
    bridge: &str,
    peer: &str,
    method: &str,
    groups: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let method_trim = method.trim();
    if method_trim.is_empty() {
        return Err("--method required (e.g. `--method tool.web_fetch`)".into());
    }
    let mut url = format!(
        "{}/v1/policy/simulate?peer={}&method={}",
        bridge.trim_end_matches('/'),
        urlencoding(peer),
        urlencoding(method_trim),
    );
    let groups_trim = groups.trim();
    if !groups_trim.is_empty() {
        url.push_str("&groups=");
        url.push_str(&urlencoding(groups_trim));
    }
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let r: PolicySimulateResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/policy/simulate body: {e} (body={body})"))?;
    let groups_label = if r.groups.is_empty() {
        "(no groups)".to_string()
    } else {
        r.groups.join(",")
    };
    println!("peer={p}  method={m}", p = r.peer, m = r.method);
    println!("groups={g}", g = groups_label);
    println!("decision={d}", d = r.decision);
    println!(
        "matched_rule={r}",
        r = r.matched_rule.as_deref().unwrap_or("-")
    );
    if !r.reason.is_empty() {
        println!("reason={r}", r = r.reason);
    }
    Ok(())
}

// W2-007e CLI mirror: GET /v1/policy/denials

#[derive(Debug, Deserialize)]
struct PolicyDenialsResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    denials: Vec<PolicyDenialRow>,
    #[serde(default)]
    count: usize,
}

#[derive(Debug, Deserialize)]
// caller_subject_id is preserved for forensic identity and
// future "--show-subject" mode; the default table is too
// narrow to display the full 32-byte fingerprint.
#[allow(dead_code)]
struct PolicyDenialRow {
    #[serde(default)]
    at: i64,
    #[serde(default)]
    method: String,
    #[serde(default)]
    caller_subject_id: String,
    #[serde(default)]
    caller_name: String,
    #[serde(default)]
    rule: String,
    #[serde(default)]
    reason: String,
}

async fn policy_denials(
    bridge: &str,
    peer: &str,
    max: usize,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/policy/denials?peer={}&max={}",
        bridge.trim_end_matches('/'),
        urlencoding(peer),
        max,
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let r: PolicyDenialsResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/policy/denials body: {e} (body={body})"))?;
    if r.denials.is_empty() {
        println!(
            "(no denials in ring on peer '{p}' — count={c})",
            p = r.peer,
            c = r.count
        );
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let when_h = "when";
    let method_h = "method";
    let caller_h = "caller";
    let rule_h = "rule";
    let reason_h = "reason";
    println!("{when_h:<10}  {method_h:<28}  {caller_h:<16}  {rule_h:<24}  {reason_h}");
    for d in &r.denials {
        let age = (now - d.at).max(0);
        let when = format_age(age);
        let method = truncate(&d.method, 28);
        let caller = truncate(&d.caller_name, 16);
        let rule = truncate(&d.rule, 24);
        println!(
            "{when:<10}  {method:<28}  {caller:<16}  {rule:<24}  {reason}",
            when = when,
            method = method,
            caller = caller,
            rule = rule,
            reason = d.reason,
        );
    }
    println!("count={}", r.count);
    Ok(())
}

/// W7-SEARCH: `relix-cli ops session-search` — hits the
/// bridge's `GET /v1/memory/sessions/search` and renders the
/// results as a table (timestamp, role, session_id short,
/// content preview). `--json` dumps the raw response body.
async fn session_search(
    bridge: &str,
    query: &str,
    subject_id: &str,
    limit: usize,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if query.trim().is_empty() {
        return Err("query is required".into());
    }
    let limit = limit.min(100);
    let mut url = format!(
        "{}/v1/memory/sessions/search?q={}&limit={}",
        bridge.trim_end_matches('/'),
        urlencoding(query),
        limit,
    );
    if !subject_id.is_empty() {
        url.push_str(&format!("&subject_id={}", urlencoding(subject_id)));
    }
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode session-search body: {e} (body={body})"))?;
    let results = parsed
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let total = parsed.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    if results.is_empty() {
        println!("(no matches for {query:?}; total={total})");
        return Ok(());
    }
    println!(
        "{:<19}  {:<10}  {:<14}  preview",
        "timestamp", "role", "session"
    );
    for r in &results {
        let ts_unix = r
            .get("timestamp_unix")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let role = r.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let session = r.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        let content = r.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let session_short = if session.len() > 14 {
            &session[..14]
        } else {
            session
        };
        let preview: String = content.chars().take(100).collect();
        println!(
            "{:<19}  {:<10}  {:<14}  {}",
            format_unix_ts(ts_unix),
            truncate(role, 10),
            session_short,
            preview,
        );
    }
    println!("total={total}");
    Ok(())
}

/// Render a unix timestamp as UTC `YYYY-MM-DD HH:MM:SS`. We
/// stick to UTC (no local-offset lookup) so the output is
/// deterministic regardless of the operator's TZ — the
/// dashboard already renders the local-time variant.
fn format_unix_ts(unix: i64) -> String {
    if unix <= 0 {
        return "—".to_string();
    }
    let Ok(odt) = time::OffsetDateTime::from_unix_timestamp(unix) else {
        return format!("unix={unix}");
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        odt.year(),
        odt.month() as u8,
        odt.day(),
        odt.hour(),
        odt.minute(),
        odt.second(),
    )
}

/// W2-007e: minimal "Xs ago" / "Xm ago" / "Xh ago" formatter.
fn format_age(secs: i64) -> String {
    if secs < 60 {
        return format!("{secs}s ago");
    }
    if secs < 3600 {
        return format!("{}m ago", secs / 60);
    }
    format!("{}h ago", secs / 3600)
}

/// Tiny URL-encoding helper. `urlencoding` crate isn't in
/// the workspace and the operator-facing values here are
/// short identifiers — manual escaping is fine.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let is_safe = matches!(
            b,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~'
                | b',' | b'/'
        );
        if is_safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

// ── PH-CRON-CLI: cron scheduler subcommands ────────────────

async fn cron_run(cmd: CronCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        CronCmd::List {
            bridge,
            subject_id,
            json,
        } => cron_list(&bridge, &subject_id, json).await,
        CronCmd::Create {
            bridge,
            name,
            schedule,
            prompt,
            subject_id,
            flow_template,
        } => {
            cron_create(
                &bridge,
                &name,
                &schedule,
                &prompt,
                &subject_id,
                &flow_template,
            )
            .await
        }
        CronCmd::Trigger { bridge, job_id } => cron_trigger(&bridge, &job_id).await,
        CronCmd::Delete { bridge, job_id } => cron_delete(&bridge, &job_id).await,
        CronCmd::Enable { bridge, job_id } => cron_set_enabled(&bridge, &job_id, true).await,
        CronCmd::Disable { bridge, job_id } => cron_set_enabled(&bridge, &job_id, false).await,
    }
}

async fn cron_list(
    bridge: &str,
    subject_id: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = if subject_id.trim().is_empty() {
        format!("{base}/v1/cron/jobs")
    } else {
        format!("{base}/v1/cron/jobs?subject_id={}", urlencode(subject_id))
    };
    let body = http_get_string(&url).await?;
    if json {
        println!("{body}");
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    let jobs = parsed
        .get("jobs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if jobs.is_empty() {
        println!("(no jobs)");
        return Ok(());
    }
    println!(
        "{:<16} {:<20} {:<24} {:<8} next",
        "job_id", "name", "schedule", "enabled"
    );
    for j in jobs {
        let id = j.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
        let name = j.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let schedule = j.get("schedule").and_then(|v| v.as_str()).unwrap_or("");
        let enabled = j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let next = j.get("next_run_at").and_then(|v| v.as_i64()).unwrap_or(0);
        println!(
            "{:<16} {:<20} {:<24} {:<8} {}",
            short(id, 16),
            short(name, 20),
            short(schedule, 24),
            enabled,
            next
        );
    }
    Ok(())
}

async fn cron_create(
    bridge: &str,
    name: &str,
    schedule: &str,
    prompt: &str,
    subject_id: &str,
    flow_template: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/cron/jobs");
    let body = json!({
        "name": name,
        "schedule": schedule,
        "prompt": prompt,
        "subject_id": subject_id,
        "flow_template": flow_template,
    });
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn cron_trigger(bridge: &str, job_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/cron/jobs/{}/trigger", urlencode(job_id));
    let client = reqwest::Client::new();
    let r = client.post(&url).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn cron_delete(bridge: &str, job_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/cron/jobs/{}", urlencode(job_id));
    let client = reqwest::Client::new();
    let r = client.delete(&url).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn cron_set_enabled(
    bridge: &str,
    job_id: &str,
    enabled: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/cron/jobs/{}", urlencode(job_id));
    let client = reqwest::Client::new();
    let r = client
        .patch(&url)
        .json(&json!({ "enabled": enabled }))
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn http_get_string(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let r = client.get(url).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}").into());
    }
    Ok(text)
}

fn short(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars - 1).collect::<String>())
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── PH-AGENT-CLI: agent subcommands ────────────────────────

async fn agent_run(cmd: AgentCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        AgentCmd::List {
            bridge,
            subject_id,
            json,
        } => agent_list(&bridge, &subject_id, json).await,
        AgentCmd::Create {
            bridge,
            name,
            role,
            title,
            department,
            team,
            created_by,
            subject_id,
            risk_ceiling,
        } => {
            agent_create(
                &bridge,
                &name,
                &role,
                &title,
                &department,
                &team,
                &created_by,
                &subject_id,
                &risk_ceiling,
            )
            .await
        }
        AgentCmd::Get { bridge, agent_id } => agent_get(&bridge, &agent_id).await,
        AgentCmd::Enable { bridge, agent_id } => {
            agent_set_status(&bridge, &agent_id, "active").await
        }
        AgentCmd::Suspend { bridge, agent_id } => {
            agent_set_status(&bridge, &agent_id, "suspended").await
        }
        AgentCmd::Disable { bridge, agent_id } => {
            agent_set_status(&bridge, &agent_id, "disabled").await
        }
        AgentCmd::ApprovalsPending { bridge, json } => approvals_pending(&bridge, json).await,
        AgentCmd::ApprovalDecide {
            bridge,
            approval_id,
            decision,
            note,
            decided_by,
        } => approval_decide(&bridge, &approval_id, &decision, &note, &decided_by).await,
        AgentCmd::StandingApprovalGrant {
            bridge,
            agent_id,
            category,
            expires_in,
            note,
            path_glob,
        } => {
            standing_grant(
                &bridge,
                &agent_id,
                &category,
                &expires_in,
                &note,
                &path_glob,
            )
            .await
        }
        AgentCmd::StandingApprovalList {
            bridge,
            agent_id,
            json,
        } => standing_list(&bridge, &agent_id, json).await,
        AgentCmd::StandingApprovalRevoke {
            bridge,
            standing_id,
        } => standing_revoke(&bridge, &standing_id).await,
    }
}

async fn agent_list(
    bridge: &str,
    subject_id: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = if subject_id.trim().is_empty() {
        format!("{base}/v1/agents")
    } else {
        format!("{base}/v1/agents?subject_id={}", urlencode(subject_id))
    };
    let body = http_get_string(&url).await?;
    if json {
        println!("{body}");
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    let rows = parsed
        .get("agents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no agents)");
        return Ok(());
    }
    println!(
        "{:<24} {:<24} {:<16} {:<10}",
        "agent_id", "name", "role", "status"
    );
    for a in rows {
        let id = a.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let role = a.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let status = a.get("status").and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "{:<24} {:<24} {:<16} {:<10}",
            short(id, 24),
            short(name, 24),
            short(role, 16),
            short(status, 10)
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn agent_create(
    bridge: &str,
    name: &str,
    role: &str,
    title: &str,
    department: &str,
    team: &str,
    created_by: &str,
    subject_id: &str,
    risk_ceiling: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/agents");
    let body = json!({
        "name": name,
        "role": role,
        "title": title,
        "department": department,
        "team": team,
        "created_by": created_by,
        "subject_id": subject_id,
        "risk_ceiling": risk_ceiling,
    });
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn agent_get(bridge: &str, agent_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/agents/{}", urlencode(agent_id));
    let body = http_get_string(&url).await?;
    println!("{body}");
    Ok(())
}

async fn agent_set_status(
    bridge: &str,
    agent_id: &str,
    status: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/agents/{}", urlencode(agent_id));
    let client = reqwest::Client::new();
    let r = client
        .patch(&url)
        .json(&json!({ "status": status }))
        .send()
        .await?;
    let s = r.status();
    let text = r.text().await?;
    if !s.is_success() {
        eprintln!("error: HTTP {s}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn approvals_pending(bridge: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/approvals");
    let body = http_get_string(&url).await?;
    if json {
        println!("{body}");
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    let rows = parsed
        .get("approvals")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no pending approvals)");
        return Ok(());
    }
    println!(
        "{:<22} {:<24} {:<32} reason",
        "approval_id", "agent_id", "method"
    );
    for a in rows {
        let id = a.get("approval_id").and_then(|v| v.as_str()).unwrap_or("");
        let agent = a.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
        let method = a.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let reason = a.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "{:<22} {:<24} {:<32} {}",
            short(id, 22),
            short(agent, 24),
            short(method, 32),
            reason
        );
    }
    Ok(())
}

async fn approval_decide(
    bridge: &str,
    approval_id: &str,
    decision: &str,
    note: &str,
    decided_by: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/approvals/{}/decide", urlencode(approval_id));
    let client = reqwest::Client::new();
    let r = client
        .post(&url)
        .json(&json!({ "decision": decision, "note": note, "decided_by": decided_by }))
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn standing_grant(
    bridge: &str,
    agent_id: &str,
    category: &str,
    expires_in: &str,
    note: &str,
    path_glob: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let secs = parse_duration_secs(expires_in)
        .ok_or_else(|| format!("bad --expires-in: `{expires_in}` (use 30m / 2h / 1d / 7d)"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let expires_at = now + secs;
    let base = bridge.trim_end_matches('/');
    let url = format!(
        "{base}/v1/agents/{}/standing-approvals",
        urlencode(agent_id)
    );
    let mut body = json!({
        "category": category,
        "expires_at": expires_at,
        "note": note,
    });
    if !path_glob.is_empty() {
        body["path_glob"] = json!(path_glob);
    }
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn standing_list(
    bridge: &str,
    agent_id: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!(
        "{base}/v1/agents/{}/standing-approvals",
        urlencode(agent_id)
    );
    let body = http_get_string(&url).await?;
    if json {
        println!("{body}");
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    let rows = parsed
        .get("standing")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no standing approvals)");
        return Ok(());
    }
    println!(
        "{:<22} {:<16} {:<32} {:<10} {:<12} expires",
        "standing_id", "category", "path", "calls", "cost_micros"
    );
    for s in rows {
        let id = s.get("standing_id").and_then(|v| v.as_str()).unwrap_or("");
        let cat = s
            .get("match_category")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let path = s
            .get("match_path_glob")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let exp = s.get("expires_at").and_then(|v| v.as_i64()).unwrap_or(0);
        let calls_used = s.get("calls_used").and_then(|v| v.as_i64()).unwrap_or(0);
        let max_calls = s.get("max_calls").and_then(|v| v.as_i64());
        let cost_used = s
            .get("cost_used_micros")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let max_cost = s.get("max_cost_micros").and_then(|v| v.as_i64());
        let calls = max_calls
            .map(|max| format!("{calls_used}/{max}"))
            .unwrap_or_else(|| format!("{calls_used}/-"));
        let cost = max_cost
            .map(|max| format!("{cost_used}/{max}"))
            .unwrap_or_else(|| format!("{cost_used}/-"));
        println!(
            "{:<22} {:<16} {:<32} {:<10} {:<12} {}",
            short(id, 22),
            short(cat, 16),
            short(path, 32),
            short(&calls, 10),
            short(&cost, 12),
            exp
        );
    }
    Ok(())
}

async fn standing_revoke(
    bridge: &str,
    standing_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/standing-approvals/{}", urlencode(standing_id));
    let client = reqwest::Client::new();
    let r = client.delete(&url).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

/// Parse a duration string like `30m` / `2h` / `1d` / `7d`
/// into seconds. Returns `None` on a malformed input.
fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    let last = s.chars().last()?;
    if !"smhdw".contains(last) {
        return None;
    }
    let prefix = &s[..s.len() - last.len_utf8()];
    let n: i64 = prefix.parse().ok()?;
    if n <= 0 {
        return None;
    }
    let mult = match last {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        'w' => 7 * 86400,
        _ => unreachable!(),
    };
    Some(n * mult)
}

// ── PH-MSG-CLI: messaging subcommands ──────────────────────

async fn msg_run(cmd: MsgCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        MsgCmd::Send {
            bridge,
            from,
            to,
            subject,
            body,
            thread_id,
            reply_to_message_id,
            ttl_secs,
        } => {
            msg_send_call(
                &bridge,
                &from,
                &to,
                &subject,
                &body,
                &thread_id,
                &reply_to_message_id,
                ttl_secs,
            )
            .await
        }
        MsgCmd::Inbox {
            bridge,
            subject_id,
            include_read,
            limit,
            json,
        } => msg_inbox(&bridge, &subject_id, include_read, limit, json).await,
        MsgCmd::Read {
            bridge,
            message_id,
            reader_subject_id,
        } => msg_read(&bridge, &message_id, &reader_subject_id).await,
        MsgCmd::Thread {
            bridge,
            thread_id,
            subject_id,
            json,
        } => msg_thread(&bridge, &thread_id, &subject_id, json).await,
        MsgCmd::Delete {
            bridge,
            message_id,
            subject_id,
        } => msg_delete(&bridge, &message_id, &subject_id).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn msg_send_call(
    bridge: &str,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    thread_id: &str,
    reply_to_message_id: &str,
    ttl_secs: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/messages");
    let mut payload = json!({
        "from_subject_id": from,
        "to_subject_id": to,
        "subject": subject,
        "body": body,
    });
    if !thread_id.is_empty() {
        payload["thread_id"] = json!(thread_id);
    }
    if !reply_to_message_id.is_empty() {
        payload["reply_to_message_id"] = json!(reply_to_message_id);
    }
    if ttl_secs > 0 {
        payload["ttl_secs"] = json!(ttl_secs);
    }
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&payload).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn msg_inbox(
    bridge: &str,
    subject_id: &str,
    include_read: bool,
    limit: usize,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!(
        "{base}/v1/messages/inbox/{}?limit={limit}&include_read={}",
        urlencode(subject_id),
        if include_read { 1 } else { 0 }
    );
    let body = http_get_string(&url).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    let rows = parsed
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no messages)");
        return Ok(());
    }
    println!(
        "{:<18} {:<14} {:<20} {:<8} preview",
        "message_id", "from", "subject", "status"
    );
    for m in rows {
        let id = m.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
        let from = m
            .get("from_subject_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let subj = m.get("subject").and_then(|v| v.as_str()).unwrap_or("");
        let status = m.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let preview = m.get("body_preview").and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "{:<18} {:<14} {:<20} {:<8} {}",
            short(id, 18),
            short(from, 14),
            short(subj, 20),
            short(status, 8),
            preview
        );
    }
    Ok(())
}

async fn msg_read(
    bridge: &str,
    message_id: &str,
    reader_subject_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/messages/{}/read", urlencode(message_id));
    let client = reqwest::Client::new();
    let r = client
        .post(&url)
        .json(&json!({ "reader_subject_id": reader_subject_id }))
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn msg_thread(
    bridge: &str,
    thread_id: &str,
    subject_id: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!(
        "{base}/v1/messages/thread/{}?subject_id={}",
        urlencode(thread_id),
        urlencode(subject_id)
    );
    let body = http_get_string(&url).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    let rows = parsed
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(empty thread)");
        return Ok(());
    }
    for m in rows {
        let from = m
            .get("from_subject_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let preview = m.get("body_preview").and_then(|v| v.as_str()).unwrap_or("");
        let status = m.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let sent_at = m.get("sent_at").and_then(|v| v.as_i64()).unwrap_or(0);
        println!("[{sent_at}] {} ({status}): {preview}", short(from, 14));
    }
    Ok(())
}

async fn msg_delete(
    bridge: &str,
    message_id: &str,
    subject_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/messages/{}", urlencode(message_id));
    let client = reqwest::Client::new();
    let r = client
        .delete(&url)
        .json(&json!({ "subject_id": subject_id }))
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

// ── PH-DELEGATE-CLI: delegation subcommands ────────────────

async fn delegate_run(cmd: DelegateCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        DelegateCmd::Spawn {
            bridge,
            parent_task_id,
            goal,
            context,
            target_subject_id,
            depth,
        } => {
            delegate_spawn(
                &bridge,
                &parent_task_id,
                &goal,
                &context,
                &target_subject_id,
                depth,
            )
            .await
        }
        DelegateCmd::Result {
            bridge,
            child_task_id,
        } => delegate_result(&bridge, &child_task_id).await,
        DelegateCmd::Cancel {
            bridge,
            child_task_id,
            reason,
        } => delegate_cancel(&bridge, &child_task_id, &reason).await,
        DelegateCmd::List {
            bridge,
            parent_task_id,
            json,
        } => delegate_list(&bridge, &parent_task_id, json).await,
    }
}

async fn delegate_spawn(
    bridge: &str,
    parent_task_id: &str,
    goal: &str,
    context: &str,
    target_subject_id: &str,
    depth: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/delegate/spawn");
    let body = json!({
        "parent_task_id": parent_task_id,
        "goal": goal,
        "context": context,
        "target_subject_id": target_subject_id,
        "depth": depth,
    });
    let client = reqwest::Client::new();
    let r = client.post(&url).json(&body).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn delegate_result(
    bridge: &str,
    child_task_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/delegate/result/{}", urlencode(child_task_id));
    let body = http_get_string(&url).await?;
    println!("{body}");
    Ok(())
}

async fn delegate_cancel(
    bridge: &str,
    child_task_id: &str,
    reason: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/delegate/cancel/{}", urlencode(child_task_id));
    let client = reqwest::Client::new();
    let r = client
        .post(&url)
        .json(&json!({ "reason": reason }))
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {text}");
        std::process::exit(1);
    }
    println!("{text}");
    Ok(())
}

async fn delegate_list(
    bridge: &str,
    parent_task_id: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/delegate/list/{}", urlencode(parent_task_id));
    let body = http_get_string(&url).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    let rows = parsed
        .get("delegations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no delegations)");
        return Ok(());
    }
    println!("{:<16} {:<32} {:<10} created", "child", "goal", "status");
    for d in rows {
        let id = d
            .get("child_task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let goal = d.get("goal_preview").and_then(|v| v.as_str()).unwrap_or("");
        let status = d.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let created = d.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0);
        println!(
            "{:<16} {:<32} {:<10} {}",
            short(id, 16),
            short(goal, 32),
            short(status, 10),
            created
        );
    }
    Ok(())
}

// ── plugin subcommands ──────────────────────────────────────

async fn plugin_run(cmd: PluginCmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        PluginCmd::List { bridge, json } => plugin_list_cmd(&bridge, json).await,
        PluginCmd::Status {
            bridge,
            plugin_id,
            json,
        } => plugin_status_cmd(&bridge, &plugin_id, json).await,
        PluginCmd::Reload { bridge, plugin_id } => plugin_reload_cmd(&bridge, &plugin_id).await,
        PluginCmd::Disable { bridge, plugin_id } => plugin_disable_cmd(&bridge, &plugin_id).await,
    }
}

async fn plugin_list_cmd(bridge: &str, json_out: bool) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let body = http_get_string(&format!("{base}/v1/plugins")).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let plugins = v
        .get("plugins")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if plugins.is_empty() {
        println!("(no plugins)");
        return Ok(());
    }
    println!(
        "{:<18} {:<24} {:<10} {:<12} caps",
        "plugin_id", "name", "version", "status"
    );
    for p in plugins {
        let id = p.get("plugin_id").and_then(|x| x.as_str()).unwrap_or("");
        let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let version = p.get("version").and_then(|x| x.as_str()).unwrap_or("");
        let status = p.get("status").and_then(|x| x.as_str()).unwrap_or("");
        let caps = p
            .get("capabilities_count")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        println!(
            "{:<18} {:<24} {:<10} {:<12} {caps}",
            short(id, 18),
            short(name, 24),
            short(version, 10),
            short(status, 12)
        );
    }
    Ok(())
}

async fn plugin_status_cmd(
    bridge: &str,
    plugin_id: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let body = http_get_string(&format!("{base}/v1/plugins/{plugin_id}")).await?;
    if json_out {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    println!(
        "plugin_id      {}",
        v.get("plugin_id").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "name           {}",
        v.get("name").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "version        {}",
        v.get("version").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "status         {}",
        v.get("status").and_then(|x| x.as_str()).unwrap_or("")
    );
    println!(
        "registered_at  {}",
        v.get("registered_at").and_then(|x| x.as_i64()).unwrap_or(0)
    );
    let last = v.get("last_seen_at").and_then(|x| x.as_i64());
    println!(
        "last_seen_at   {}",
        last.map(|t| t.to_string()).unwrap_or_else(|| "—".into())
    );
    let caps = v
        .get("capabilities")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    println!("capabilities   {caps}");
    let err = v
        .get("error_message")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if !err.is_empty() {
        println!("error_message  {err}");
    }
    Ok(())
}

async fn plugin_reload_cmd(
    bridge: &str,
    plugin_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/plugins/{plugin_id}/reload");
    let client = reqwest::Client::new();
    let r = client.post(&url).send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    println!("{body}");
    Ok(())
}

async fn plugin_disable_cmd(
    bridge: &str,
    plugin_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/plugins/{plugin_id}/disable");
    let client = reqwest::Client::new();
    let r = client.post(&url).send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    println!("{body}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_unix_ts_renders_utc_iso_like() {
        // Unix epoch + 1 day = 1970-01-02 00:00:00.
        assert_eq!(format_unix_ts(86_400), "1970-01-02 00:00:00");
        // Sentinel for "no timestamp".
        assert_eq!(format_unix_ts(0), "—");
        assert_eq!(format_unix_ts(-1), "—");
    }

    /// Minimal mock bridge: records request headers, answers any
    /// path with 200. Lets a test assert the auth-gated probe carries
    /// `Authorization: Bearer <token>`.
    async fn spawn_header_recorder() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let seen: std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&buf);
                    let mut headers = std::collections::HashMap::new();
                    for l in text.split("\r\n").skip(1) {
                        if l.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = l.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    seen.lock().unwrap().push(headers);
                    let body = "{}";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (addr, seen)
    }

    #[tokio::test]
    async fn smoke_helpers_attach_bearer_token() {
        // AC: auth-gated smoke steps must carry
        // `Authorization: Bearer <token>`. Pins both the GET and POST
        // helpers smoke uses.
        let (addr, seen) = spawn_header_recorder().await;
        http_get_auth(&format!("{addr}/v1/topology"), Some("smoke-tok"))
            .await
            .unwrap();
        http_post_json_auth(
            &format!("{addr}/v1/chat/completions"),
            "{}",
            Some("smoke-tok"),
        )
        .await
        .unwrap();
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 2, "expected one GET and one POST");
        assert!(
            reqs.iter().all(|h| h
                .get("authorization")
                .map(|v| v.eq_ignore_ascii_case("Bearer smoke-tok"))
                .unwrap_or(false)),
            "every auth-gated smoke step must send Authorization: Bearer"
        );
    }

    #[tokio::test]
    async fn http_get_without_token_omits_authorization_header() {
        // Public steps (and auth-disabled meshes) must not invent a
        // header. None in -> no Authorization out.
        let (addr, seen) = spawn_header_recorder().await;
        http_get(&format!("{addr}/health")).await.unwrap();
        let reqs = seen.lock().unwrap();
        assert!(
            reqs.iter().all(|h| !h.contains_key("authorization")),
            "no token resolved must mean no Authorization header"
        );
    }

    #[test]
    fn urlencoding_escapes_session_search_special_chars() {
        // Spaces, ampersands, equals signs in a query must
        // round-trip safely. The encoder follows the
        // RFC-3986-unreserved set + we whitelist `/` and `,`
        // for path-shaped values.
        assert_eq!(urlencoding("hello world"), "hello%20world");
        assert_eq!(urlencoding("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencoding("simple"), "simple");
    }

    // PART 6: the `parse_typical_health_body` and
    // `parse_empty_providers` tests were removed alongside
    // `providers_health` — the bridge endpoint
    // (`/v1/providers/health`) was deleted in the prior security
    // session and the CLI no longer dispatches against it.

    #[test]
    fn parse_events_response() {
        let body = r#"{
            "items": [
                {"task_id": "abc123",
                 "event_id": 5,
                 "event_type": "task.retry_requested",
                 "payload": "raw payload",
                 "summary": "[retry] requested (#2/5)"}
            ],
            "next_cursor": 5
        }"#;
        let r: EventsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.next_cursor, 5);
        assert_eq!(r.items[0].summary, "[retry] requested (#2/5)");
    }

    // PART 6: `parse_route_test_response` was removed alongside
    // `route_test` (same reason as the providers-health tests
    // above — the bridge endpoint
    // `/v1/providers/route_test` was deleted in the prior
    // security session).

    #[test]
    fn parse_stuck_response() {
        let body = r#"{
            "items": [
                {"task_id": "abcd1234abcd1234abcd1234abcd1234",
                 "title": "long-running task",
                 "started_at": 1700000000,
                 "age_secs": 1234}
            ],
            "count": 1,
            "threshold_secs": 300
        }"#;
        let s: StuckResponse = serde_json::from_str(body).unwrap();
        assert_eq!(s.count, 1);
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.items[0].age_secs, 1234);
    }

    #[test]
    fn parse_topology_for_capabilities() {
        // PH-DASH3-CLI: minimal topology body the capabilities
        // subcommand needs. Aliases optional; methods required;
        // freshness propagated.
        let body = r#"{
            "peers": [
                {
                    "alias": "tool",
                    "node_id": "abc",
                    "node_type": "tool",
                    "node_name": "t",
                    "manifest_version": 1,
                    "capability_count": 2,
                    "methods": ["tool.web_fetch", "tool.web_search"],
                    "last_refreshed_at": 1,
                    "last_refreshed_secs_ago": 5,
                    "freshness": "fresh"
                }
            ],
            "generated_at": 0
        }"#;
        let t: TopologyResponse = serde_json::from_str(body).unwrap();
        assert_eq!(t.peers.len(), 1);
        assert_eq!(t.peers[0].methods.len(), 2);
        assert_eq!(t.peers[0].freshness, "fresh");
    }

    // ── W2-006d: dispatch_stats CLI sparkline ──────────────────

    #[test]
    fn ascii_sparkline_empty_returns_dash() {
        assert_eq!(ascii_sparkline(&[]), "-");
    }

    #[test]
    fn ascii_sparkline_flat_renders_low_bars() {
        // All-equal samples normalize to the max bar.
        let s = ascii_sparkline(&[5, 5, 5, 5]);
        assert_eq!(s.chars().count(), 4);
        // Every bar at full height since v == max for all.
        assert!(s.chars().all(|c| c == '█'));
    }

    #[test]
    fn ascii_sparkline_renders_one_char_per_sample() {
        let s = ascii_sparkline(&[1, 10, 5, 20, 8]);
        assert_eq!(s.chars().count(), 5);
        // The peak (20) should map to the tallest bar.
        assert!(s.contains('█'));
    }

    #[test]
    fn format_age_renders_buckets() {
        assert_eq!(format_age(0), "0s ago");
        assert_eq!(format_age(59), "59s ago");
        assert_eq!(format_age(60), "1m ago");
        assert_eq!(format_age(3599), "59m ago");
        assert_eq!(format_age(3600), "1h ago");
        assert_eq!(format_age(36000), "10h ago");
    }

    #[test]
    fn urlencoding_passes_safe_chars() {
        assert_eq!(urlencoding("tool.web_fetch"), "tool.web_fetch");
        assert_eq!(urlencoding("a,b/c-d.e_f~g"), "a,b/c-d.e_f~g");
    }

    #[test]
    fn urlencoding_escapes_specials() {
        assert_eq!(urlencoding(" "), "%20");
        assert_eq!(urlencoding("?"), "%3F");
        assert_eq!(urlencoding("="), "%3D");
        assert_eq!(urlencoding("&"), "%26");
    }

    #[test]
    fn policy_simulate_resp_parses() {
        let body = r#"{
            "peer": "tool",
            "method": "tool.web_fetch",
            "groups": ["chat-users", "operators"],
            "decision": "allow",
            "matched_rule": "web_fetch_chat",
            "reason": "explicit allow"
        }"#;
        let r: PolicySimulateResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.decision, "allow");
        assert_eq!(r.matched_rule.as_deref(), Some("web_fetch_chat"));
        assert_eq!(r.groups.len(), 2);
    }

    #[test]
    fn policy_simulate_resp_handles_missing_rule() {
        let body = r#"{
            "peer": "tool",
            "method": "tool.unknown",
            "groups": [],
            "decision": "deny",
            "matched_rule": null,
            "reason": "default deny"
        }"#;
        let r: PolicySimulateResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.decision, "deny");
        assert!(r.matched_rule.is_none());
    }

    #[test]
    fn port_from_bridge_default() {
        assert_eq!(
            port_from_bridge(crate::defaults::DEFAULT_BRIDGE_URL),
            crate::defaults::DEFAULT_BRIDGE_PORT
        );
    }

    #[test]
    fn port_from_bridge_custom() {
        assert_eq!(port_from_bridge("http://localhost:8080"), 8080);
        assert_eq!(port_from_bridge("https://example.com:443"), 443);
    }

    #[test]
    fn port_from_bridge_with_trailing_slash() {
        assert_eq!(port_from_bridge("http://127.0.0.1:19791/"), 19791);
    }

    #[test]
    fn port_from_bridge_falls_back_when_unparseable() {
        // No port → default.
        assert_eq!(port_from_bridge("http://example.com"), 19791);
        // Garbage → default.
        assert_eq!(port_from_bridge("not a url"), 19791);
    }

    #[test]
    fn models_resp_parses() {
        let body = r#"{
            "object": "list",
            "data": [
                {"id":"relix-mock", "object":"model", "created":0,
                 "owned_by":"relix", "description":"mock route"},
                {"id":"relix-openai", "object":"model", "created":0,
                 "owned_by":"relix", "description":"openai route"}
            ]
        }"#;
        let r: ModelsResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.data.len(), 2);
        assert_eq!(r.data[0].id, "relix-mock");
        assert_eq!(r.data[1].description, "openai route");
    }

    #[test]
    fn agent_memory_resp_parses() {
        let body = r#"{
            "peer": "memory",
            "subject_id": "abc123",
            "agent_memory": "rust uses cargo§python uses pip",
            "user_memory":  "prefers concise replies",
            "agent_chars": 30,
            "user_chars":  23
        }"#;
        let r: AgentMemoryResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.peer, "memory");
        assert_eq!(r.subject_id, "abc123");
        assert!(r.agent_memory.contains("rust uses cargo"));
        assert!(r.user_memory.contains("prefers concise"));
        assert_eq!(r.agent_chars, 30);
        assert_eq!(r.user_chars, 23);
    }

    #[test]
    fn agent_memory_resp_parses_empty_fields() {
        // Missing fields default to empty strings / zero counts —
        // first-call agents have no memory yet but the response
        // still parses cleanly.
        let body = r#"{
            "peer": "memory",
            "subject_id": "abc"
        }"#;
        let r: AgentMemoryResp = serde_json::from_str(body).unwrap();
        assert!(r.agent_memory.is_empty());
        assert!(r.user_memory.is_empty());
        assert_eq!(r.agent_chars, 0);
        assert_eq!(r.user_chars, 0);
    }

    #[test]
    fn csv_field_passthrough_for_safe_strings() {
        assert_eq!(csv_field("task.created"), "task.created");
        assert_eq!(csv_field(""), "");
        assert_eq!(csv_field("simple summary"), "simple summary");
    }

    #[test]
    fn csv_field_quotes_commas() {
        assert_eq!(csv_field("a,b,c"), "\"a,b,c\"");
    }

    #[test]
    fn csv_field_doubles_internal_quotes() {
        assert_eq!(csv_field("he said \"hi\""), "\"he said \"\"hi\"\"\"");
    }

    #[test]
    fn csv_field_quotes_newlines_and_crs() {
        assert_eq!(csv_field("line one\nline two"), "\"line one\nline two\"");
        assert_eq!(
            csv_field("line one\r\nline two"),
            "\"line one\r\nline two\""
        );
    }

    #[test]
    fn policy_denials_resp_parses() {
        let body = r#"{
            "peer": "tool",
            "denials": [
                {"at": 1716, "method": "tool.web_fetch",
                 "caller_subject_id": "abcd", "caller_name": "bob",
                 "rule": "default_deny", "reason": "no rule matched"}
            ],
            "count": 1
        }"#;
        let r: PolicyDenialsResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.count, 1);
        assert_eq!(r.denials.len(), 1);
        assert_eq!(r.denials[0].method, "tool.web_fetch");
        assert_eq!(r.denials[0].caller_name, "bob");
    }

    #[test]
    fn dispatch_stats_row_parses_recent_latencies() {
        // Forward-compat: the JSON may or may not include
        // recent_latencies. Both shapes parse.
        let with_field = r#"{
            "method": "tool.web_fetch",
            "invocations": 5, "errors": 0, "denied": 0, "unknown_method": 0,
            "last_invoked_at": 100, "latency_samples": 5,
            "last_elapsed_ms": 10, "max_elapsed_ms": 25, "mean_elapsed_ms": 12,
            "recent_latencies": [10, 15, 12, 25, 8]
        }"#;
        let r: DispatchStatsRow = serde_json::from_str(with_field).unwrap();
        assert_eq!(r.recent_latencies, vec![10, 15, 12, 25, 8]);

        let without_field = r#"{
            "method": "tool.web_fetch",
            "invocations": 5, "errors": 0, "denied": 0, "unknown_method": 0,
            "last_invoked_at": 100, "latency_samples": 5,
            "last_elapsed_ms": 10, "max_elapsed_ms": 25, "mean_elapsed_ms": 12
        }"#;
        let r2: DispatchStatsRow = serde_json::from_str(without_field).unwrap();
        assert!(r2.recent_latencies.is_empty());
    }
}
