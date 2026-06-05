//! Bridge config + shared `AppState`.
//!
//! Parsed once at startup; the resulting [`AppState`] is cloned into each
//! axum handler. Identity bundle, client key, peers file, and the SOL flow
//! template are all loaded here so the request path stays I/O-free.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::flow_runner::PeersFile;
use relix_runtime::manifest::{ManifestCache, MeshClient};

use crate::BridgeError;

/// Top-level bridge config (TOML-loaded).
#[derive(Clone, Debug, Deserialize)]
pub struct BridgeConfig {
    pub bridge: BridgeSection,
    pub identity: IdentitySection,
    pub transport: TransportSection,
    pub flow: FlowSection,
    /// Optional OpenAI-compatible shim. Absent ⇒ `/v1/*` routes are 404.
    #[serde(default)]
    pub openai_compat: Option<OpenAiCompatSection>,
    /// Optional SSE settings shared by `/chat/stream` and the streaming
    /// variant of `/v1/chat/completions`.
    #[serde(default)]
    pub sse: SseSection,
    /// Optional Coordinator integration. When present, every chat request
    /// is persisted as a Task on the named peer. When absent or when the
    /// peer is unreachable, the bridge degrades gracefully (warn + skip
    /// persistence; the user's request still executes).
    #[serde(default)]
    pub coordinator: Option<CoordinatorSection>,
    /// Optional per-principal HTTP + WS rate-limit budgets. Missing
    /// section ⇒ the documented defaults in `crate::rate_limit`
    /// (60/min AI, 120/min dashboard polls, 30/min task mutations,
    /// 5 concurrent WS sockets).
    #[serde(default)]
    pub mesh: MeshSection,
    /// Optional observability wiring (W7). Absent / disabled means
    /// the bridge's ObservabilityContext stays buffer-only and the
    /// OTLP exporter never spawns. Same shape as the controller's
    /// `[observability.otel]`.
    #[serde(default)]
    pub observability: Option<BridgeObservabilitySection>,
    /// PART 5: multi-tenant auth configuration. Absent /
    /// default keeps the pre-PART-5 single-tenant behaviour
    /// (`multi_tenant_mode = false`, `tenant_bindings` empty,
    /// `trusted_internal_origins = ["127.0.0.1", "::1"]`).
    /// See [`AuthSection`].
    #[serde(default)]
    pub auth: AuthSection,
    /// P3: logging-stream redaction posture. Absent ⇒ defaults
    /// to `redact_stream = true`. See [`LoggingSection`].
    #[serde(default)]
    pub logging: LoggingSection,
}

/// `[logging]` — P3 log-stream posture. Today only governs the
/// SSE log-stream endpoint's redaction policy.
///
/// ```toml
/// [logging]
/// # Default true. Set false to disable redaction; the bridge
/// # logs a WARN at startup so the operator's posture is
/// # visible in the boot log.
/// redact_stream = true
/// ```
#[derive(Clone, Debug, Deserialize)]
pub struct LoggingSection {
    /// P3: when `true` (the default), every log line streamed
    /// out via `GET /v1/logs/stream` is run through
    /// [`relix_core::redact::redact_secrets`] so API keys,
    /// bearer tokens, JWTs, AWS credentials, and the other
    /// well-known secret shapes are masked before they reach
    /// the dashboard. When `false`, raw log content is sent
    /// — operator's explicit posture choice; the bridge logs
    /// a startup WARN so the choice is visible.
    #[serde(default = "default_redact_stream")]
    pub redact_stream: bool,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            redact_stream: default_redact_stream(),
        }
    }
}

fn default_redact_stream() -> bool {
    true
}

/// `[auth]` — multi-tenant binding + trusted-origin
/// configuration. PART 5 of the tenant-isolation rollout.
///
/// ```toml
/// [auth]
/// multi_tenant_mode = false
/// trusted_internal_origins = ["127.0.0.1", "::1"]
///
/// [auth.tenant_bindings]
/// # First 8 chars of API key → tenant_id.
/// "deadbeef" = "acme"
/// "cafef00d" = "globex"
/// ```
///
/// `multi_tenant_mode = true` makes a missing tenant binding
/// fail-closed with HTTP 401; `false` keeps the single-tenant
/// pre-PART-5 behaviour.
#[derive(Clone, Debug, Deserialize, Default)]
pub struct AuthSection {
    /// When `true`, every authenticated request MUST resolve
    /// to a tenant via [`AuthSection::tenant_bindings`] or
    /// the HTTP layer returns 401. When `false`, missing
    /// bindings fall through to the single-tenant default
    /// (legacy behaviour). Defaults to `false`.
    #[serde(default)]
    pub multi_tenant_mode: bool,
    /// IP addresses whose `X-Relix-Tenant` header is honoured
    /// (e.g. trusted reverse-proxy / control-plane). Requests
    /// from any other source IP have the header IGNORED. The
    /// default whitelist accepts loopback only so external
    /// callers cannot inject a tenant id by hand.
    #[serde(default = "default_trusted_origins")]
    pub trusted_internal_origins: Vec<String>,
    /// Map of the first 8 characters of an API key to the
    /// tenant id the credential belongs to. Operators set
    /// this in their `[auth.tenant_bindings]` block; the
    /// bridge token's prefix matches an entry here to derive
    /// the canonical tenant for every request.
    #[serde(default)]
    pub tenant_bindings: std::collections::HashMap<String, String>,
    /// SEC PART 3: operator-configured setup token guarding
    /// `GET /v1/auth/token`. Pre-fix path returned the
    /// bridge token to any loopback caller; the bootstrap
    /// surface now requires a matching `Authorization:
    /// Bearer <setup_token>` (or `X-Relix-Setup-Token`)
    /// header. Falls back to the `RELIX_SETUP_TOKEN` env
    /// var when this field is unset.
    #[serde(default)]
    pub setup_token: Option<String>,
}

fn default_trusted_origins() -> Vec<String> {
    vec!["127.0.0.1".to_string(), "::1".to_string()]
}

/// `[observability]` for the bridge. Carries the OTel block; future
/// observability layers (Prom metrics, etc.) extend this struct.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BridgeObservabilitySection {
    #[serde(default)]
    pub otel: Option<BridgeOtelSection>,
}

/// `[observability.otel]` — operator-friendly OTel config. Same
/// shape as controller-side `OtelConfigToml`; duplicated here so
/// the bridge keeps its config schema self-contained.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BridgeOtelSection {
    #[serde(default)]
    pub enabled: bool,
    /// OTLP/HTTP `/v1/traces` URL.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub service_name: Option<String>,
    /// Event types to opt into export. Empty means nothing is
    /// exported even when enabled.
    #[serde(default)]
    pub events: Vec<String>,
}

/// Bridge-level container for the `[mesh]` block. Only carries
/// rate-limit settings today; future mesh-wide knobs (alternative
/// transport pin lists, default deadlines, etc.) can extend this
/// section without breaking existing operator configs.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MeshSection {
    #[serde(default)]
    pub rate_limits: Option<crate::rate_limit::RateLimitConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CoordinatorSection {
    /// Peer alias in `peers.toml` (e.g. `"coordinator"`). Bridge uses
    /// `MeshClient::call(alias, ...)` to send `task.*` calls, so the
    /// reconnect-on-drop behaviour applies for free.
    pub alias: String,
    /// When true, chat execution requires durable task creation. The bridge
    /// fails startup if the alias is not discovered, and each chat request
    /// fails before dispatch if `task.create` cannot return a task id.
    /// Defaults to false for local/dev fail-soft compatibility.
    #[serde(default)]
    pub required: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BridgeSection {
    /// `127.0.0.1:9100` by default. Always loopback in alpha.
    pub listen_addr: String,
    /// Path to the bridge-owned secrets file. Defaults to
    /// `<data_dir>/bridge-secrets.toml` when unset. Holds the
    /// operator-supplied AI provider keys + Telegram bot token
    /// configured via the dashboard's settings pages. Local to
    /// one bridge process; written at mode 0600 on POSIX;
    /// gitignored. See docs/dashboard-redesign.md for the
    /// security model.
    #[serde(default)]
    pub secrets_path: Option<PathBuf>,
    /// Path to the bridge auth-token file. Defaults to
    /// `~/.relix/bridge-token` when unset (and to
    /// `./bridge-token` if `$HOME` / `%USERPROFILE%` is
    /// unresolvable). The file is generated on first boot
    /// (256 bits, hex-encoded) and persisted at restrictive
    /// permissions. Every state-changing route requires this
    /// token via `Authorization: Bearer <token>`. See
    /// `docs/security.md`.
    #[serde(default)]
    pub token_path: Option<PathBuf>,
    /// Optional path to the layered memory store
    /// (`memory.layered.db`). When configured, the bridge
    /// opens the SQLite file directly and serves the
    /// `/v1/memory/records/*` inspector endpoints from it.
    /// When `None`, those endpoints return a clear
    /// "not configured" 503 — the bridge does NOT auto-open
    /// the file from a derived path because the bridge and
    /// the memory controller may run on different hosts /
    /// containers. Operators co-locating the two set this
    /// explicitly. Read-only operations (list / show / stats)
    /// are safe against a concurrently-writing memory node
    /// thanks to WAL mode; `invalidate` flips `valid_to` and
    /// is also WAL-safe.
    #[serde(default)]
    pub memory_db_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IdentitySection {
    /// The signed IdentityBundle this bridge presents to mesh responders.
    pub bundle_path: PathBuf,
    /// 32-byte secret used as the local libp2p PeerId AND as the signer for
    /// the per-flow event log records.
    pub client_key_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TransportSection {
    /// Path to the peer alias map (`relix_runtime::flow_runner::PeersFile`).
    pub peers_path: PathBuf,
    /// Per-call deadline. Default 30 s.
    #[serde(default = "default_deadline")]
    pub deadline_secs: i64,
    /// Per-flow event log directory; defaults to `RELIX_DATA_DIR` discovery.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
}

fn default_deadline() -> i64 {
    30
}

#[derive(Clone, Debug, Deserialize)]
pub struct FlowSection {
    /// Path to the SOL chat template file. Two placeholders are substituted
    /// at request time: `{{SESSION}}` and `{{MESSAGE}}`.
    pub template_path: PathBuf,
    /// Optional second template that adds a `tool.web_fetch` step before the
    /// AI call (M9). Three placeholders: `{{SESSION}}`, `{{MESSAGE}}`,
    /// `{{TOOL_URL}}`. When unset, `/chat_with_tool` is 404 and the OpenAI
    /// shim never auto-routes to it.
    #[serde(default)]
    pub tool_template_path: Option<PathBuf>,
    /// RELIX-2 step 5: optional streaming-chat template. Same placeholders
    /// as `template_path` (`{{SESSION}}` + `{{MESSAGE}}`); the template
    /// MUST use `remote_call_stream("ai", "ai.chat.stream", ...)` so the
    /// flow opens a streaming substream. When set AND the request has
    /// `stream: true`, the bridge wires a chunk observer that ships
    /// tokens to the SSE response BEFORE the SOL VM finishes. When
    /// unset, `stream: true` falls back to the legacy chunk-sliced path
    /// (no behaviour change for existing installs).
    #[serde(default)]
    pub streaming_template_path: Option<PathBuf>,
}

/// Bridge-level SSE knobs. See `docs/streaming-and-openai-shim.md`.
#[derive(Clone, Debug, Deserialize)]
pub struct SseSection {
    /// Bytes per SSE chunk when slicing the final reply. Default 32.
    #[serde(default = "default_chunk_bytes")]
    pub chunk_bytes: usize,
    /// Inter-chunk delay in milliseconds, simulating progressive delivery.
    /// Default 25 ms. Set to 0 for an immediate flush.
    #[serde(default = "default_chunk_delay_ms")]
    pub chunk_delay_ms: u64,
}

impl Default for SseSection {
    fn default() -> Self {
        Self {
            chunk_bytes: default_chunk_bytes(),
            chunk_delay_ms: default_chunk_delay_ms(),
        }
    }
}

fn default_chunk_bytes() -> usize {
    32
}

fn default_chunk_delay_ms() -> u64 {
    25
}

/// OpenAI-compatible shim configuration.
///
/// The shim translates `POST /v1/chat/completions` requests into the same
/// SOL chat flow the native `/chat` endpoint uses. Provider keys never live
/// here — provider selection still happens inside the AI node.
#[derive(Clone, Debug, Deserialize)]
pub struct OpenAiCompatSection {
    /// Models advertised by `GET /v1/models`. Each entry maps a client-facing
    /// id (e.g. `relix-mock`) to a free-form description. The bridge does NOT
    /// route based on the chosen id — provider selection is on the AI node.
    /// The list is purely advisory so OpenAI-compatible clients see something
    /// in their model picker.
    #[serde(default)]
    pub models: Vec<OpenAiModelEntry>,
    /// Default model id returned in responses when the client did not supply
    /// one. Empty ⇒ falls back to the first `models` entry, then to `"relix"`.
    #[serde(default)]
    pub default_model: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OpenAiModelEntry {
    pub id: String,
    #[serde(default)]
    pub description: String,
}

/// In-memory app state: validated config + preloaded identity bundle + peers.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<BridgeConfig>,
    pub identity_bundle: Bundle,
    /// SEC PART 2: the bridge's 32-byte libp2p secret key
    /// lives inside `Zeroizing` so the bytes are wiped from
    /// the heap when the last AppState clone is dropped
    /// (bridge shutdown). Each per-request clone of AppState
    /// gets its own zeroizing copy.
    pub client_key: zeroize::Zeroizing<[u8; 32]>,
    pub peers: PeersFile,
    pub template: String,
    /// Pre-validated tool-flow template when `[flow] tool_template_path` is
    /// set. `None` ⇒ `/chat_with_tool` returns 404 and the OpenAI shim
    /// never auto-routes to the tool flow.
    pub tool_template: Option<String>,
    /// RELIX-2 step 5: pre-validated streaming-chat template
    /// when `[flow] streaming_template_path` is set. `None` ⇒
    /// `POST /v1/chat/completions stream:true` falls back to
    /// the legacy chunk-sliced path (consumes the full reply
    /// first, then splits into SSE chunks). When set, the
    /// bridge wires a chunk observer that pipes tokens
    /// through the SOL VM directly to the SSE response.
    pub streaming_template: Option<String>,
    /// Capability discovery cache populated at bridge startup (M10). Empty
    /// when discovery failed; the bridge stays up and static aliases continue
    /// to work. Read by `/v1/models` and (optionally) by the flow runner's
    /// `capability:` resolver.
    pub manifest_cache: Arc<ManifestCache>,
    /// Long-lived libp2p client with peers pre-dialled. `Some` after a
    /// successful discovery pass; `None` if discovery failed (in which case
    /// FlowRunner falls back to the per-request ephemeral peer path).
    pub mesh_client: Option<Arc<MeshClient>>,
    /// Coordinator integration. `Some` when `[coordinator] alias` is set
    /// in bridge config AND the mesh client is up. Used fail-soft from
    /// flow.rs — every method on `TaskRecorder` warns-and-skips on
    /// failure so chat requests never block on coordinator availability.
    pub task_recorder: Option<crate::task_recorder::TaskRecorder>,
    /// Wall-clock unix seconds at which AppState was built (≈ process
    /// start). Surfaced by `/v1/health` so dashboards + load balancers
    /// can derive uptime without a separate /metrics endpoint.
    pub started_at: i64,
    /// Bridge-owned secrets (provider keys, Telegram token). Persisted
    /// to `bridge-secrets.toml` at the path resolved in `try_new`. The
    /// dashboard's settings pages read + write through this handle.
    /// See `crates/relix-web-bridge/src/secrets.rs` for the persistence
    /// contract.
    pub secrets: crate::secrets::SecretsHandle,
    /// Bridge-process-local runtime metrics (active SSE streams,
    /// total streams opened). Exposed via `/v1/health`. Counters
    /// reset on bridge restart — see
    /// `crates/relix-web-bridge/src/metrics.rs`.
    pub stream_metrics: std::sync::Arc<crate::metrics::StreamMetrics>,
    /// Bridge-process-local lifecycle event log (peer joins,
    /// freshness transitions, drops). A background polling
    /// task populates it. Exposed via `/v1/topology/events`.
    /// Ring resets on bridge restart — see
    /// `crates/relix-web-bridge/src/lifecycle.rs`.
    pub lifecycle_log: std::sync::Arc<crate::lifecycle::LifecycleLog>,
    /// Bridge-side operator intervention audit ring (M57).
    /// Every mutating operator-facing call appends an entry.
    /// In-memory ring + best-effort JSONL append at
    /// `<data_dir>/bridge-intervention.log.jsonl` when a
    /// data_dir is configured. Exposed via
    /// `/v1/intervention/recent`. See
    /// `crates/relix-web-bridge/src/intervention_audit.rs`.
    pub intervention_audit: std::sync::Arc<crate::intervention_audit::InterventionAudit>,
    /// PH-BRIDGE-MCP-AUDIT: bridge-side audit ring for
    /// `POST /v1/mcp/invoke`. Bounded in-memory; surfaced via
    /// `GET /v1/mcp/audit`. Resets on bridge restart. See
    /// `crates/relix-web-bridge/src/mcp_audit.rs`.
    pub mcp_audit: std::sync::Arc<crate::mcp_audit::McpAuditRing>,
    /// Bridge HTTP bearer token. Generated on first boot and
    /// persisted to `~/.relix/bridge-token` (or the configured
    /// override). Every state-changing route requires it via the
    /// `auth_middleware` in `crate::auth`. Read-only on the live
    /// state — rotation requires a bridge restart.
    pub bridge_token: crate::auth::BridgeToken,
    /// Dashboard operator-login state: the durable Argon2id admin
    /// credential + the in-memory session table. Backs the
    /// `/v1/auth/{status,setup,login,logout,me}` endpoints and lets the
    /// auth middleware admit a request carrying a valid session cookie.
    pub dashboard_auth: crate::dashboard_auth::DashboardAuth,
    /// SEC PART 3: operator-configured setup token guarding
    /// the `GET /v1/auth/token` bootstrap endpoint. `None`
    /// when neither `[auth] setup_token` is in `bridge.toml`
    /// nor `RELIX_SETUP_TOKEN` is in the environment, in
    /// which case the bootstrap surface returns HTTP 403 —
    /// operators must opt in.
    pub setup_token: Option<String>,
    /// Host portion of the listen address (e.g. `127.0.0.1`).
    /// Used by the CSRF origin guard to verify the request's
    /// `Origin` header matches the bridge's own host.
    pub bridge_host: String,
    /// TCP port of the listen address. Used by the CSRF origin
    /// guard for the same reason as `bridge_host`.
    pub bridge_port: u16,
    /// Per-principal rate-limit buckets and the WS concurrency
    /// gate. Shared with both the HTTP middleware and the
    /// WebSocket handler. See `crate::rate_limit`.
    pub rate_limits: crate::rate_limit::RateLimits,
    /// Real-time log ring + broadcast surface. Installed as a
    /// tracing-subscriber layer at process startup; consumed by
    /// the dashboard's Section 18 (`GET /v1/logs/stream`).
    pub log_ring: crate::logs::LogRing,
    /// Multi-agent handoff audit ring. Bounded in-memory
    /// (`HANDOFF_RING_CAP = 100`); resets on bridge restart.
    /// Populated via `POST /v1/guardrails/handoffs`; read via
    /// the same path's GET. See
    /// `crates/relix-web-bridge/src/guardrails.rs`.
    pub handoff_audit: crate::guardrails::HandoffAuditRing,
    /// JIT secret store loaded from `RELIX_*` env vars at
    /// bridge startup. Backs the `/v1/secrets/available`
    /// endpoint (NAMES ONLY — values never leave the
    /// process). See
    /// `crates/relix-runtime/src/nodes/execution/secrets.rs`.
    pub jit_secrets: std::sync::Arc<relix_runtime::nodes::execution::secrets::SecretStore>,
    /// Agent access broker. Empty by default — operators
    /// configure per-agent allow / deny / rate-limit policies
    /// via `[[execution.agents]]` in the bridge config.
    /// Backs the `/v1/agents/access` endpoint. See
    /// `crates/relix-runtime/src/nodes/execution/broker.rs`.
    pub access_broker: std::sync::Arc<relix_runtime::nodes::execution::broker::AgentAccessBroker>,
    /// Discoverable tool registry. Built empty here and
    /// reassigned in `main.rs` after the startup discovery pass
    /// via `crate::tools::registry_from_manifest`, which reads
    /// the tool node's advertised capability descriptors out of
    /// the manifest cache. Stays empty when no tool peer was
    /// discovered. Backs `/v1/tools` (list), `/v1/tools/search`
    /// (keyword), and `/v1/tools/manifest` (signed). See
    /// `crates/relix-runtime/src/nodes/tool/registry.rs`.
    pub tool_registry: std::sync::Arc<relix_runtime::nodes::tool::registry::ToolRegistry>,
    /// Two-sink observability. Metadata events for every
    /// model call land in Sink A; content (prompt /
    /// response / tool output) lands in Sink B with a
    /// short retention window. See
    /// `crates/relix-runtime/src/observability/sinks.rs`.
    pub observability: relix_runtime::observability::ObservabilityContext,
    /// W7: optional OTLP exporter handle. Set when
    /// `[observability.otel]` is enabled in bridge config and
    /// the exporter was successfully built. `main.rs` consults
    /// this Arc to spawn the periodic flush loop. `None` means
    /// no OTel egress and no flush loop.
    pub otel_exporter: Option<std::sync::Arc<relix_runtime::observability::OtelExporter>>,
    /// Four-layer memory store backing the
    /// `/v1/memory/records/*` inspector endpoints.
    /// `Some` when `[bridge] memory_db_path` is configured AND
    /// the SQLite open succeeded; `None` otherwise. Inspector
    /// handlers return 503 on `None` with a clear message
    /// telling the operator how to wire it.
    pub layered_memory:
        Option<std::sync::Arc<relix_runtime::nodes::memory::schema::LayeredMemoryStore>>,
}

/// Resolve the bridge auth-token path from config, falling back to
/// `~/.relix/bridge-token` (the operator's home Relix dir). Shared by the
/// running bridge and the local admin-reset CLI so both target the SAME
/// `dashboard-admin.json` (which sits next to this token file).
pub fn resolve_bridge_token_path(cfg: &BridgeConfig) -> PathBuf {
    cfg.bridge.token_path.clone().unwrap_or_else(|| {
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        match std::env::var_os(home_var) {
            Some(h) => PathBuf::from(h).join(".relix").join("bridge-token"),
            None => PathBuf::from("bridge-token"),
        }
    })
}

/// The default admin-credential path when no bridge config is supplied:
/// `~/.relix/dashboard-admin.json`. Mirrors [`resolve_bridge_token_path`]'s
/// fallback so a bare `reset-admin` (no `--config`) targets the same file
/// the bridge uses by default.
pub fn default_admin_path() -> PathBuf {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let token = match std::env::var_os(home_var) {
        Some(h) => PathBuf::from(h).join(".relix").join("bridge-token"),
        None => PathBuf::from("bridge-token"),
    };
    crate::dashboard_auth::admin_path_for_token(&token)
}

impl AppState {
    pub fn try_new(cfg: BridgeConfig) -> Result<Self, BridgeError> {
        let bundle_bytes = std::fs::read(&cfg.identity.bundle_path).map_err(|e| {
            BridgeError::Config(format!(
                "read identity bundle {}: {e}",
                cfg.identity.bundle_path.display()
            ))
        })?;
        let identity_bundle: Bundle = codec::decode(&bundle_bytes)
            .map_err(|e| BridgeError::Config(format!("decode identity bundle: {e}")))?;

        let client_key = load_or_generate_client_key(&cfg.identity.client_key_path)?;

        let peers = PeersFile::from_path(&cfg.transport.peers_path)
            .map_err(|e| BridgeError::Config(format!("peers: {e}")))?;

        let template = std::fs::read_to_string(&cfg.flow.template_path).map_err(|e| {
            BridgeError::Config(format!(
                "read flow template {}: {e}",
                cfg.flow.template_path.display()
            ))
        })?;
        if !template.contains("{{SESSION}}") || !template.contains("{{MESSAGE}}") {
            return Err(BridgeError::Config(
                "flow template must contain {{SESSION}} and {{MESSAGE}} placeholders".to_string(),
            ));
        }

        let tool_template = if let Some(path) = cfg.flow.tool_template_path.as_ref() {
            let text = std::fs::read_to_string(path).map_err(|e| {
                BridgeError::Config(format!("read tool flow template {}: {e}", path.display()))
            })?;
            if !text.contains("{{SESSION}}")
                || !text.contains("{{MESSAGE}}")
                || !text.contains("{{TOOL_URL}}")
            {
                return Err(BridgeError::Config(
                    "tool flow template must contain {{SESSION}}, {{MESSAGE}} and {{TOOL_URL}} placeholders"
                        .to_string(),
                ));
            }
            Some(text)
        } else {
            None
        };

        // RELIX-2 step 5: optional streaming template. Must
        // call `remote_call_stream("ai", "ai.chat.stream", ...)`
        // — the bridge enforces a sanity check on the source
        // so operators that set the config without using the
        // streaming opcode see a startup error instead of a
        // silent fallback at request time.
        let streaming_template = if let Some(path) = cfg.flow.streaming_template_path.as_ref() {
            let text = std::fs::read_to_string(path).map_err(|e| {
                BridgeError::Config(format!(
                    "read streaming flow template {}: {e}",
                    path.display()
                ))
            })?;
            if !text.contains("{{SESSION}}") || !text.contains("{{MESSAGE}}") {
                return Err(BridgeError::Config(
                    "streaming flow template must contain {{SESSION}} and {{MESSAGE}} placeholders"
                        .to_string(),
                ));
            }
            // The template MUST exercise the streaming
            // dispatcher. SOL templates do this via the
            // `remote_call_stream` builtin; YAML templates
            // (`.yml` / `.yaml`) use the `stream:` step which
            // lowers to the same opcode. Either marker
            // satisfies the check.
            let is_yaml = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("yml") || e.eq_ignore_ascii_case("yaml"))
                .unwrap_or(false);
            let invokes_stream = if is_yaml {
                text.contains("stream:")
            } else {
                text.contains("remote_call_stream")
            };
            if !invokes_stream {
                return Err(BridgeError::Config(
                    "streaming flow template must invoke the streaming dispatcher — \
                     SOL: `remote_call_stream(...)`, YAML: `stream:` step — otherwise the \
                     chunk observer never fires and stream:true is no better than the unary path"
                        .to_string(),
                ));
            }
            Some(text)
        } else {
            None
        };

        // Resolve the secrets file path. Default location is
        // `<data_dir>/bridge-secrets.toml` when `data_dir` is
        // set, falling back to `bridge-secrets.toml` in the
        // current working directory otherwise. Operators
        // override via `[bridge] secrets_path`.
        let secrets_path = cfg.bridge.secrets_path.clone().unwrap_or_else(|| {
            match cfg.transport.data_dir.as_ref() {
                Some(d) => d.join("bridge-secrets.toml"),
                None => PathBuf::from("bridge-secrets.toml"),
            }
        });
        let initial_secrets = crate::secrets::BridgeSecrets::load_or_empty(&secrets_path);
        let secrets = crate::secrets::SecretsHandle::new(initial_secrets, secrets_path);

        // Resolve the bridge auth-token path. Default location is
        // `~/.relix/bridge-token` so it sits next to the operator's
        // other Relix state, regardless of which workspace the
        // bridge was launched from.
        let token_path = resolve_bridge_token_path(&cfg);
        let bridge_token = crate::auth::BridgeToken::load_or_generate(&token_path)
            .map_err(|e| BridgeError::Config(format!("bridge-token: {e}")))?;
        // Dashboard operator-login state. The admin credential is stored
        // next to the bridge token; sessions are in-memory.
        let dashboard_auth = crate::dashboard_auth::DashboardAuth::from_token_path(&token_path);

        // SEC PART 3: resolve the setup token guarding
        // `/v1/auth/token`. `[auth] setup_token` wins; the
        // env var is the fallback. `None` keeps the
        // bootstrap surface refusing every caller until the
        // operator opts in (the bootstrap handler returns
        // HTTP 403 in that state).
        let setup_token = cfg
            .auth
            .setup_token
            .clone()
            .or_else(|| std::env::var("RELIX_SETUP_TOKEN").ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Parse listen_addr early so the auth/CSRF middleware can
        // compare the request's Origin host against the bridge's
        // own. Failures here mirror the late-parse error in main.rs
        // so misconfiguration is rejected at startup either way.
        let listen: std::net::SocketAddr = cfg
            .bridge
            .listen_addr
            .parse()
            .map_err(|e| BridgeError::Config(format!("listen_addr: {e}")))?;
        let bridge_host = listen.ip().to_string();
        let bridge_port = listen.port();

        // Intervention audit ring. JSONL persistence lives
        // next to the data_dir when one is configured; in-
        // memory only otherwise (so single-binary smoke tests
        // don't need a writable disk).
        let intervention_path = cfg
            .transport
            .data_dir
            .as_ref()
            .map(|d| d.join("bridge-intervention.log.jsonl"));
        let intervention_audit =
            crate::intervention_audit::InterventionAudit::new(intervention_path);

        // Snapshot rate-limit config before `cfg` moves into the
        // shared Arc on the next line — the limiter owns its own
        // copy of just the budgets.
        let rate_limit_cfg = cfg.mesh.rate_limits.clone().unwrap_or_default();
        let memory_db_path = cfg.bridge.memory_db_path.clone();
        // Snapshot the observability section before `cfg` is
        // moved into the AppState literal. The W7 OTel exporter
        // is built from this snapshot below so the producer
        // (ObservabilityContext) and the flush handle share one
        // Arc.
        let observability_cfg = cfg.observability.clone();
        let mut state = Self {
            cfg: Arc::new(cfg),
            identity_bundle,
            client_key,
            peers,
            template,
            tool_template,
            streaming_template,
            manifest_cache: Arc::new(ManifestCache::new()),
            mesh_client: None,
            task_recorder: None,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            secrets,
            stream_metrics: crate::metrics::StreamMetrics::new(),
            lifecycle_log: crate::lifecycle::LifecycleLog::new(),
            intervention_audit,
            mcp_audit: Arc::new(crate::mcp_audit::McpAuditRing::default()),
            bridge_token,
            dashboard_auth,
            setup_token,
            bridge_host,
            bridge_port,
            rate_limits: crate::rate_limit::RateLimits::new(rate_limit_cfg),
            log_ring: crate::logs::LogRing::new(),
            handoff_audit: crate::guardrails::HandoffAuditRing::new(),
            jit_secrets: std::sync::Arc::new(
                relix_runtime::nodes::execution::secrets::SecretStore::from_env(),
            ),
            access_broker: std::sync::Arc::new(
                relix_runtime::nodes::execution::broker::AgentAccessBroker::empty(),
            ),
            tool_registry: crate::tools::empty_registry(),
            observability: {
                // Placeholder; the real composition happens
                // immediately after the AppState literal so the
                // OTel exporter and ObservabilityContext share a
                // single Arc.
                relix_runtime::observability::ObservabilityContext::in_memory()
            },
            otel_exporter: None,
            layered_memory: open_layered_memory(&memory_db_path),
        };

        // W7: build the OTel exporter once and thread the SAME
        // Arc into both `observability` (the producer-facing
        // context) and `otel_exporter` (the handle main.rs uses
        // to spawn the flush loop). Reusing one Arc means
        // `record_event` and `flush` operate on the same buffer.
        if let Some(exporter) = build_bridge_otel(&observability_cfg) {
            let obs = std::mem::replace(
                &mut state.observability,
                relix_runtime::observability::ObservabilityContext::in_memory(),
            );
            state.observability = obs.with_otel(exporter.clone());
            state.otel_exporter = Some(exporter);
        }
        Ok(state)
    }
}

/// Build an `Arc<OtelExporter>` from the bridge's optional
/// observability section. Returns `None` unless the OTel block
/// is enabled AND has an endpoint URL — same fail-safe posture
/// as the controller-side `build_otel_config`.
pub(crate) fn build_bridge_otel(
    obs: &Option<BridgeObservabilitySection>,
) -> Option<std::sync::Arc<relix_runtime::observability::OtelExporter>> {
    let otel = obs.as_ref()?.otel.as_ref()?;
    if !otel.enabled || otel.endpoint.is_none() {
        return None;
    }
    let mut runtime_cfg = relix_runtime::observability::OtelConfig {
        enabled: true,
        endpoint_url: otel.endpoint.clone(),
        ..relix_runtime::observability::OtelConfig::default()
    };
    if let Some(s) = otel.service_name.as_deref()
        && !s.is_empty()
    {
        runtime_cfg.service_name = s.to_string();
    }
    let mut events = runtime_cfg.events.clone();
    for e in &otel.events {
        events = events.enable(e);
    }
    runtime_cfg.events = events;
    tracing::info!(
        endpoint = ?otel.endpoint,
        events = ?otel.events,
        "bridge observability: OTLP exporter enabled"
    );
    Some(std::sync::Arc::new(
        relix_runtime::observability::OtelExporter::new(runtime_cfg),
    ))
}

/// Open the four-layer memory store for the inspector
/// endpoints. Returns `None` when the operator did not
/// configure a path OR when the SQLite open failed; either
/// way, inspector handlers serve a clear 503.
fn open_layered_memory(
    path: &Option<PathBuf>,
) -> Option<std::sync::Arc<relix_runtime::nodes::memory::schema::LayeredMemoryStore>> {
    let path = path.as_ref()?;
    match relix_runtime::nodes::memory::schema::LayeredMemoryStore::open(path) {
        Ok(s) => Some(std::sync::Arc::new(s)),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "memory inspector: failed to open layered store; inspector endpoints will return 503"
            );
            None
        }
    }
}

/// Load the bridge's local libp2p secret from disk, or generate a new one if
/// the file does not exist. Mirrors `controller_runtime::load_or_generate_key`
/// so operators do not have to remember a manual `relix-cli` step before
/// first start. The file is gitignored (`dev-keys/*.key`).
pub fn load_or_generate_client_key(
    path: &Path,
) -> Result<zeroize::Zeroizing<[u8; 32]>, BridgeError> {
    if path.exists() {
        // SEC PART 2: the raw 32-byte disk read lives inside
        // Zeroizing so the bytes are wiped immediately after
        // the array copy.
        let bytes: zeroize::Zeroizing<Vec<u8>> =
            zeroize::Zeroizing::new(std::fs::read(path).map_err(|e| {
                BridgeError::Config(format!("read client key {}: {e}", path.display()))
            })?);
        if bytes.len() != 32 {
            return Err(BridgeError::Config(format!(
                "{}: expected 32-byte secret key, got {}",
                path.display(),
                bytes.len()
            )));
        }
        let mut out = zeroize::Zeroizing::new([0u8; 32]);
        out.copy_from_slice(&bytes);
        Ok(out)
    } else {
        use rand::RngCore;
        let mut out = zeroize::Zeroizing::new([0u8; 32]);
        rand::rngs::OsRng.fill_bytes(&mut *out);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| BridgeError::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::write(path, *out).map_err(|e| {
            BridgeError::Config(format!("write client key {}: {e}", path.display()))
        })?;
        // POSIX chmod 0600 + Windows icacls strip-inheritance.
        // Best-effort: a permission failure doesn't block boot
        // (operator still has a working key) but doctor will
        // surface the looseness on its next run.
        let _ = crate::os_secure::restrict_to_current_user(path);
        tracing::info!(path = %path.display(), "generated new bridge client key");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_config_parses_minimal() {
        let toml_str = r#"
            [bridge]
            listen_addr = "127.0.0.1:9100"

            [identity]
            bundle_path     = "dev-keys/bridge.aic"
            client_key_path = "dev-keys/bridge.key"

            [transport]
            peers_path    = "configs/peers-chained.toml"
            deadline_secs = 30

            [flow]
            template_path = "flows/chat_template.sol"
        "#;
        let cfg: BridgeConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.bridge.listen_addr, "127.0.0.1:9100");
        assert_eq!(cfg.transport.deadline_secs, 30);
        assert!(cfg.openai_compat.is_none());
        assert_eq!(cfg.sse.chunk_bytes, 32);
    }

    #[test]
    fn bridge_config_parses_openai_compat_and_sse() {
        let toml_str = r#"
            [bridge]
            listen_addr = "127.0.0.1:9100"

            [identity]
            bundle_path     = "dev-keys/bridge.aic"
            client_key_path = "dev-keys/bridge.key"

            [transport]
            peers_path = "configs/peers-chained.toml"

            [flow]
            template_path = "flows/chat_template.sol"

            [sse]
            chunk_bytes    = 16
            chunk_delay_ms = 5

            [openai_compat]
            default_model = "relix-mock"
            [[openai_compat.models]]
            id          = "relix-mock"
            description = "Deterministic mock through the Relix mesh"
            [[openai_compat.models]]
            id          = "relix-anthropic"
        "#;
        let cfg: BridgeConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.sse.chunk_bytes, 16);
        assert_eq!(cfg.sse.chunk_delay_ms, 5);
        let oa = cfg.openai_compat.expect("openai_compat section");
        assert_eq!(oa.default_model, "relix-mock");
        assert_eq!(oa.models.len(), 2);
        assert_eq!(oa.models[0].id, "relix-mock");
    }

    // ── W7: bridge-side OTel wiring ──────────────────────────────

    #[test]
    fn bridge_config_parses_required_coordinator_flag() {
        let toml_str = r#"
            [bridge]
            listen_addr = "127.0.0.1:9100"

            [identity]
            bundle_path     = "dev-keys/bridge.aic"
            client_key_path = "dev-keys/bridge.key"

            [transport]
            peers_path = "configs/peers-chained.toml"

            [flow]
            template_path = "flows/chat_template.sol"

            [coordinator]
            alias = "coordinator"
            required = true
        "#;
        let cfg: BridgeConfig = toml::from_str(toml_str).expect("parse");
        let coord = cfg.coordinator.expect("coordinator section");
        assert_eq!(coord.alias, "coordinator");
        assert!(coord.required);
    }

    #[test]
    fn bridge_config_parses_observability_otel_section() {
        let toml_str = r#"
            [bridge]
            listen_addr = "127.0.0.1:9100"

            [identity]
            bundle_path     = "dev-keys/bridge.aic"
            client_key_path = "dev-keys/bridge.key"

            [transport]
            peers_path    = "configs/peers-chained.toml"
            deadline_secs = 30

            [flow]
            template_path = "flows/chat_template.sol"

            [observability.otel]
            enabled = true
            endpoint = "http://localhost:4318/v1/traces"
            service_name = "relix-bridge"
            events = ["model_call"]
        "#;
        let cfg: BridgeConfig = toml::from_str(toml_str).expect("parse");
        let otel = cfg
            .observability
            .as_ref()
            .and_then(|o| o.otel.as_ref())
            .expect("otel section present");
        assert!(otel.enabled);
        assert_eq!(
            otel.endpoint.as_deref(),
            Some("http://localhost:4318/v1/traces")
        );
        assert_eq!(otel.events, vec!["model_call".to_string()]);
    }

    #[test]
    fn build_bridge_otel_returns_none_when_disabled_or_no_endpoint() {
        // Section missing → None.
        assert!(super::build_bridge_otel(&None).is_none());
        // Enabled but no endpoint → None (fail-safe).
        let no_endpoint = BridgeObservabilitySection {
            otel: Some(BridgeOtelSection {
                enabled: true,
                endpoint: None,
                ..Default::default()
            }),
        };
        assert!(super::build_bridge_otel(&Some(no_endpoint)).is_none());
        // Endpoint present but disabled → None.
        let disabled = BridgeObservabilitySection {
            otel: Some(BridgeOtelSection {
                enabled: false,
                endpoint: Some("http://x".into()),
                ..Default::default()
            }),
        };
        assert!(super::build_bridge_otel(&Some(disabled)).is_none());
    }

    #[test]
    fn build_bridge_otel_returns_exporter_when_enabled_and_endpoint_set() {
        let section = BridgeObservabilitySection {
            otel: Some(BridgeOtelSection {
                enabled: true,
                endpoint: Some("http://localhost:4318/v1/traces".into()),
                service_name: Some("relix-bridge".into()),
                events: vec!["model_call".into()],
            }),
        };
        let exp =
            super::build_bridge_otel(&Some(section)).expect("exporter built when enabled+endpoint");
        let cfg = exp.config();
        assert!(cfg.enabled);
        assert_eq!(cfg.service_name, "relix-bridge");
        assert_eq!(
            cfg.endpoint_url.as_deref(),
            Some("http://localhost:4318/v1/traces")
        );
        assert!(cfg.events.is_enabled("model_call"));
    }

    #[tokio::test]
    async fn observability_context_with_otel_buffers_then_flush_posts() {
        // End-to-end at the AppState layer minus a real
        // BridgeConfig file: build an ObservabilityContext via
        // the same path AppState::try_new uses, push a chat
        // metadata event, drain via flush(), and confirm the
        // OTLP collector mock saw a POST. This proves the
        // producer (ObservabilityContext::record_event) and
        // the flush handle share one Arc.
        use std::io::{Read, Write};
        use std::sync::Arc as TestArc;
        use std::sync::Mutex as TestMutex;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: TestArc<TestMutex<Vec<u8>>> = TestArc::new(TestMutex::new(Vec::new()));
        let cap_clone = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).unwrap_or(0);
                if let Some(body_start) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
                    *cap_clone.lock().unwrap() = buf[body_start + 4..n].to_vec();
                }
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let url = format!("http://{addr}/v1/traces");
        let section = BridgeObservabilitySection {
            otel: Some(BridgeOtelSection {
                enabled: true,
                endpoint: Some(url),
                service_name: Some("relix-bridge".into()),
                events: vec!["model_call".into()],
            }),
        };
        let exporter = super::build_bridge_otel(&Some(section)).expect("exporter built");
        let ctx = relix_runtime::observability::ObservabilityContext::in_memory()
            .with_otel(exporter.clone());
        // Producer-side: feed a model_call through record_event.
        ctx.record_event(
            relix_runtime::observability::MetadataEvent {
                event_id: "ev-1".into(),
                session_id: "sess-1".into(),
                agent_id: "bridge".into(),
                event_type: "model_call".into(),
                timestamp_unix: 1_700_000_000,
                latency_ms: Some(120),
                token_count: None,
                cost_cents: None,
                error_type: None,
                tool_name: None,
                model_name: Some("gpt-test".into()),
                success: true,
            },
            None,
        );
        // Flush → POST hits the mock.
        let drained = exporter.flush().await;
        assert_eq!(drained.len(), 1, "exporter must buffer the event");
        // Wait a brief moment for the mock thread to record the body.
        for _ in 0..20 {
            if !captured.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let body = captured.lock().unwrap().clone();
        assert!(!body.is_empty(), "OTLP mock did not receive a POST body");
        let body_str = String::from_utf8(body).unwrap();
        assert!(
            body_str.contains("relix.model_call"),
            "body missing span name: {body_str}"
        );
    }
}
