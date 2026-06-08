//! Controller runtime — what `relix-controller`'s `main()` calls to spin up a
//! node. Loads identity + policy, builds dispatch bridge, starts libp2p,
//! registers built-in `node.health` capability, dispatches inbound RPCs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::Deserialize;
use tokio::sync::mpsc;

use relix_core::codec;
use relix_core::policy::PolicyEngine;
use relix_core::types::NodeId;

use crate::dispatch::{DispatchBridge, FnHandler, Handler, HandlerOutcome, InvocationCtx};
use crate::manifest::ManifestProvider;
use crate::transport::rpc::{self, Event as TransportEvent, Multiaddr};

/// Controller config (per-binary). Matches the TOML in `configs/`.
#[derive(Clone, Debug, Deserialize)]
pub struct ControllerConfig {
    /// `[controller]` section.
    pub controller: ControllerSection,
    /// `[identity]`.
    pub identity: IdentitySection,
    /// `[trust]`.
    pub trust: TrustSection,
    /// `[policy]`.
    pub policy: PolicySection,
    /// Optional per-node sections (memory/ai/tool/bridge). The runtime ignores
    /// unknown sections so each node-type's main can read its own typed view.
    #[serde(default)]
    #[allow(dead_code)]
    pub memory: Option<toml::Value>,
    /// AI node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub ai: Option<toml::Value>,
    /// Tool node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub tool: Option<toml::Value>,
    /// Web-bridge node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub bridge: Option<toml::Value>,
    /// Coordinator node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub coordinator: Option<toml::Value>,
    /// Telegram-channel node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub telegram: Option<toml::Value>,
    /// Discord-channel node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub discord: Option<toml::Value>,
    /// Slack-channel node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub slack: Option<toml::Value>,
    /// Email-channel node options. Present only when
    /// `node_type = "email"`.
    #[serde(default)]
    #[allow(dead_code)]
    pub email: Option<toml::Value>,
    /// Plugin-host node options. Present only when
    /// `node_type = "plugin_host"`.
    #[serde(default)]
    #[allow(dead_code)]
    pub plugin_host: Option<toml::Value>,
    /// `[reports]` — scheduled summary report config. Parsed on
    /// the coordinator node so the report loop can run alongside
    /// the retention loop. See
    /// `crates/relix-runtime/src/nodes/channels/reports.rs` for
    /// the schema; absent means no reporter spawns.
    #[serde(default)]
    pub reports: Option<toml::Value>,
    /// `[skills]` — auto-skill generation + library config.
    /// Absent means auto-generation stays off. See
    /// `crates/relix-runtime/src/nodes/ai/skills.rs`.
    #[serde(default)]
    pub skills: Option<toml::Value>,
    /// `[guardrails]` — request-time guardrail config. Carries
    /// the optional `[guardrails.input]` block; absent means
    /// no guardrails and existing controllers behave as before.
    /// See `crates/relix-runtime/src/nodes/ai/guardrails`.
    #[serde(default)]
    pub guardrails: Option<toml::Value>,
    /// `[execution]` — per-agent access policies for the
    /// dispatch broker. W2 wiring. Absent means an empty
    /// broker (every check returns Allow).
    #[serde(default)]
    pub execution: Option<ExecutionSection>,
    /// `[observability]` — root for observability-layer
    /// wiring. W7 ships the `[observability.otel]` sub-block.
    /// Absent / disabled means the OTel exporter never spawns
    /// and existing deployments stay HTTP-egress-free.
    #[serde(default)]
    pub observability: Option<ObservabilitySection>,
    /// `[metrics]` — RELIX-7.11 per-agent metrics + alert
    /// thresholds + per-model price table. Absent / disabled
    /// means the dispatch bridge runs without a metrics sink
    /// and the existing `node.dispatch.stats` counters remain
    /// the only data surface.
    #[serde(default)]
    pub metrics: Option<crate::metrics::MetricsConfig>,
    /// `[budget]` — RELIX-7.28 Part 1 cost-control caps. The
    /// per-agent + deployment limits the dispatch bridge
    /// enforces before invoking a handler. Absent / empty
    /// means the budget enforcer is dormant and the bridge
    /// keeps pre-7.28 dispatch behaviour.
    #[serde(default)]
    pub budget: Option<crate::metrics::BudgetConfig>,
    /// `[mesh_pii]` — RELIX-7.28 Part 3 PII gate. Absent /
    /// disabled keeps the bridge in pre-7.28 mode (zero
    /// scanning overhead).
    #[serde(default)]
    pub mesh_pii: Option<crate::nodes::pii_gate::MeshPiiConfig>,
    /// `[training]` — RELIX-7.15 training data pipeline. Absent
    /// / disabled keeps the AI handler running without an
    /// interaction sink — no `training.sqlite` file is opened
    /// and the six `training.*` capabilities stay unregistered.
    #[serde(default)]
    pub training: Option<crate::training::TrainingConfig>,
    /// `[knowledge]` — RELIX-7.16 agent-to-agent knowledge
    /// transfer. Absent / empty `groups` list keeps the
    /// existing memory pipeline byte-identical to the
    /// pre-7.16 build — no `knowledge.*` capabilities are
    /// registered and the AutoShareTask is not spawned.
    #[serde(default)]
    pub knowledge: Option<crate::knowledge::KnowledgeConfig>,
    /// SEC §16: `[knowledge_trust]` — receiver-side source-node
    /// binding for `knowledge.accept_shared`. Maps each trusted peer
    /// `source_node` NAME to its identity Ed25519 PUBLIC key so an
    /// inbound share's signature is bound to the claimed source node
    /// (not just "some valid key"). Absent ⇒ no peer keys known ⇒
    /// inbound mesh shares from unconfigured sources are REJECTED
    /// (fail closed) unless `allow_unbound_sources` is set.
    #[serde(default)]
    pub knowledge_trust: Option<KnowledgeTrustConfig>,
    /// `[confidence]` block — RELIX-7.19 per-step confidence
    /// scoring and fallback. Absent OR `enabled = false` keeps
    /// every node's `DispatchBridge` in pre-7.19 byte-identical
    /// mode: no scorer, no fallback engine, no
    /// `last_confidence` cell. See
    /// [`crate::confidence::ConfidenceConfig`] for the schema.
    #[serde(default)]
    pub confidence: Option<crate::confidence::ConfidenceConfig>,
    /// `[planning]` block — RELIX-7.24 Stage-1 + Stage-3
    /// multi-specialist orchestrator + critic configuration.
    /// Absent → both default to enabled with the coordinator
    /// as the AI peer. See [`crate::planning::PlanningConfig`]
    /// for the schema.
    #[serde(default)]
    pub planning: Option<crate::planning::PlanningConfig>,
    /// `[routing]` — RELIX-7.7 / 7.11 GAP 2 channel routing
    /// rules. Validated at coordinator boot against `[peers]`.
    /// Absent means every inbound channel message falls back
    /// to the legacy fixed `("ai", "ai.chat")` target.
    #[serde(default)]
    pub routing: Option<crate::nodes::coordinator::routing::RoutingConfig>,
    /// `[audit]` — GAP 23C per-tenant audit partition mirror.
    /// Absent / `partition_by_tenant = false` keeps the bridge
    /// in pre-23C mode (only the canonical signed CBOR log is
    /// written). When enabled, every finalised audit lands as
    /// a queryable SQLite row keyed by sanitised tenant id.
    #[serde(default)]
    pub audit: Option<AuditSection>,
    /// `[approval]` — GAP 15 partial: cross-cap operator
    /// approval surfaces. The current scope is the
    /// always-require allowlist; future commits can grow this
    /// section with a method-prefix wildcard set, a per-tenant
    /// override map, etc.
    #[serde(default)]
    pub approval: Option<ApprovalSection>,
    /// `[peers]` — alias → endpoint info.
    #[serde(default)]
    pub peers: std::collections::BTreeMap<String, PeerConfig>,
    /// `[agents.<name>]` — per-agent operator preferences.
    /// Today it carries the RELIX-7.15 per-agent training
    /// opt-in + PII strategy override. Empty / absent means
    /// every agent inherits the global behaviour.
    #[serde(default)]
    pub agents: std::collections::BTreeMap<String, AgentSection>,
    /// SOL session declarations (M6).
    #[serde(default)]
    #[allow(dead_code)]
    pub session: std::collections::BTreeMap<String, SessionConfig>,
    /// `[credentials]` — RELIX-7.30 PART 2 credential vault.
    /// Absent / `enabled = false` keeps the controller
    /// credential-less.
    #[serde(default)]
    pub credentials: Option<crate::credentials::CredentialsConfig>,
    /// `[session_identity]` — RELIX-7.30 PART 3 per-session
    /// token container. Holds `[session_identity.session]`.
    /// Absent leaves the DispatchBridge token-less. Named
    /// `session_identity` (not `identity`) to avoid collision
    /// with the existing org-level `[identity]` section.
    #[serde(default)]
    pub session_identity: Option<SessionIdentitySection>,
}

/// `[session_identity]` container so future commits can grow
/// this surface without breaking the TOML shape.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SessionIdentitySection {
    #[serde(default)]
    pub session: Option<crate::identity::SessionIdentityConfig>,
    /// `[session_identity.research]` — RELIX-7.18 / GAP 17
    /// research-backed identity pipeline. Absent / `enabled =
    /// false` leaves the `identity.research` cap unregistered.
    #[serde(default)]
    pub research: Option<crate::identity::research::ResearchConfig>,
    /// `[session_identity.web_search]` — search-provider
    /// surface used by the research pipeline. Required when
    /// `[session_identity.research] enabled = true`.
    #[serde(default)]
    pub web_search: Option<crate::nodes::tool::web_search::WebSearchConfig>,
}

/// `[agents.<name>]` config section. Operators add one of
/// these for any agent whose default training behaviour they
/// want to change OR to declare planning-time capabilities
/// (RELIX-7.24).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentSection {
    /// `[agents.<name>.training]` — RELIX-7.15 training opt-in
    /// + per-agent PII strategy.
    #[serde(default)]
    pub training: Option<AgentTrainingSection>,
    /// RELIX-7.24: the libp2p peer alias from `[peers]` that
    /// hosts this agent's capabilities. `None` defaults to
    /// the agent's name — operators with a 1:1
    /// agent-name → peer-alias mapping leave this unset.
    #[serde(default)]
    pub peer: Option<String>,
    /// RELIX-7.24: one-sentence human-readable description
    /// the planner shows operators + scores against the spec
    /// goal via keyword overlap.
    #[serde(default)]
    pub description: Option<String>,
    /// RELIX-7.24: explicit per-capability declarations.
    /// Each entry tells the planner "this agent handles
    /// `method` for these tags." Empty / absent → the planner
    /// falls back to the cached peer manifest from
    /// `[crate::manifest::ManifestCache]`.
    #[serde(default)]
    pub capabilities: Vec<AgentCapabilityDecl>,
}

/// RELIX-7.24: one row under `[agents.<name>] capabilities`.
/// Decoupled from `CapabilityDescriptor` so operators can
/// declare planner-facing tags + descriptions without
/// re-stating the wire-level descriptor fields the
/// node-type registration already owns.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentCapabilityDecl {
    /// Fully-qualified capability method
    /// (e.g. `"ai.chat"`, `"tool.web_search"`).
    pub method: String,
    /// Operator-supplied description shown in the planner's
    /// `planning.list_agents` output.
    #[serde(default)]
    pub description: Option<String>,
    /// Free-form keyword tags the planner scores against the
    /// spec goal. Conventional examples: `"research"`,
    /// `"code"`, `"summarise"`. Empty → tag-overlap scoring
    /// degrades to description-overlap scoring.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `[agents.<name>.training]` — per-agent training config.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AgentTrainingSection {
    /// `false` drops every interaction from this agent at the
    /// recorder's sink boundary. `None` inherits the global
    /// recorder behaviour.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// One of `"redact"`, `"pseudonymize"`, `"allow"`. When set,
    /// overrides the global `[training.pii] strategy` for this
    /// agent. The global per-type `overrides` table still
    /// applies on top of the per-agent strategy.
    #[serde(default)]
    pub pii_strategy: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ControllerSection {
    pub name: String,
    pub node_type: String,
    pub listen_port: u16,
    /// Operator-facing role flag. `"controller"` (default) runs
    /// the standard per-node-type capability surface plus a
    /// 60-second heartbeat sender to the configured router.
    /// `"router"` runs the four router.* capabilities and the
    /// stale-peer + session reaper background loops; the
    /// heartbeat sender is NOT spawned.
    #[serde(default = "default_role")]
    pub role: String,
    /// Non-router nodes: the libp2p PeerId (base58) of the
    /// designated router. Empty string / `None` disables the
    /// heartbeat sender silently — the controller still boots.
    #[serde(default)]
    pub router_peer_id: Option<String>,
    /// Router-only: seconds to retain completed/failed
    /// sessions before reaping. Running sessions never time
    /// out. Default 1800 (30 minutes).
    #[serde(default = "default_session_ttl")]
    pub session_ttl_secs: u64,
}

/// Default value for `ControllerSection::role`. Standalone fn
/// because `#[serde(default = "...")]` requires a path.
fn default_role() -> String {
    "controller".to_string()
}

/// Default value for `ControllerSection::session_ttl_secs`.
fn default_session_ttl() -> u64 {
    1800
}

#[derive(Clone, Debug, Deserialize)]
pub struct IdentitySection {
    pub key_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TrustSection {
    pub org_root_key_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PolicySection {
    pub file: PathBuf,
    /// GAP 23B: per-tenant policy directory. When set, the
    /// controller looks up `{dir}/{tenant_id}.policy.toml` on
    /// every admission check; missing per-tenant files fall
    /// through to [`Self::file`]. Absent = single-tenant
    /// behaviour (every call evaluates against `file`).
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// GAP 23B: TTL (seconds) for the per-tenant engine cache.
    /// Default 60. A value of 0 disables caching — useful in
    /// tests / dev where a policy file is edited frequently.
    #[serde(default = "default_tenant_cache_ttl")]
    pub tenant_cache_ttl_secs: u64,
}

fn default_tenant_cache_ttl() -> u64 {
    60
}

/// GAP 15 partial: `[approval]` section.
///
/// Currently carries the "always require operator approval"
/// allowlist. When `always_require_methods` is non-empty,
/// every dispatched call whose method matches one of the
/// listed names returns `APPROVAL_REQUIRED` unless the call
/// already carries an `approval_token` — even if the caller's
/// policy + (when wired) agent gate would otherwise admit.
///
/// Example:
///
/// ```toml
/// [approval]
/// always_require_methods = [
///     "tool.fs.write",
///     "tool.terminal.run",
///     "memory.bulk_export",
/// ]
/// ```
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ApprovalSection {
    /// Method names that always require an attached approval
    /// token. Order is irrelevant — the dispatch bridge checks
    /// for membership only.
    #[serde(default)]
    pub always_require_methods: Vec<String>,
    /// `[approval.delivery]` — RELIX-7.30 PART 1: out-of-band
    /// approval delivery matrix. Absent keeps every approval
    /// request on the default in-process logging-only path so
    /// existing deployments stay byte-identical.
    #[serde(default)]
    pub delivery: Option<crate::approval::ApprovalDeliveryConfig>,
    /// SQLite path for the delivery store. Defaults to
    /// `{data_dir}/approval_delivery.db` when unset.
    #[serde(default)]
    pub delivery_db_path: Option<PathBuf>,
    /// DEFERRED 1: lifetime in seconds the
    /// `coord.approval.decide` cap stamps on each freshly-minted
    /// [`crate::approval::ApprovalToken`]. `None` ⇒ the
    /// runtime default
    /// ([`crate::nodes::coordinator::agent::handlers::APPROVAL_TOKEN_TTL_DEFAULT_SECS`],
    /// 300s = 5 minutes). The controller clamps to
    /// `[APPROVAL_TOKEN_TTL_MIN_SECS, APPROVAL_TOKEN_TTL_MAX_SECS]`
    /// (`[30, 86400]`) at boot — values outside the window are
    /// quietly snapped to the nearest endpoint AND logged at
    /// INFO so the operator sees the clamp without a config
    /// reload loop.
    #[serde(default)]
    pub approval_token_ttl_secs: Option<u64>,
}

/// GAP 23C: `[audit]` section. Wires the per-tenant audit
/// partition mirror. Absent / `partition_by_tenant = false`
/// keeps the bridge in pre-23C mode.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AuditSection {
    /// When `true`, every finalised audit is mirrored to the
    /// SQLite partition store at [`Self::db_path`] keyed by
    /// sanitised tenant id. Defaults to `false` so existing
    /// deployments stay byte-identical.
    #[serde(default)]
    pub partition_by_tenant: bool,
    /// Path to the SQLite mirror file. Defaults to
    /// `{data_dir}/audit-partition.db` when unset.
    #[serde(default)]
    pub db_path: Option<PathBuf>,
}

/// `[observability]` section — top-level container for
/// observability wiring. W7 carries the `otel` block; future
/// waves can add tracing exporters / metrics endpoints
/// without re-shaping the schema.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ObservabilitySection {
    #[serde(default)]
    pub otel: Option<OtelConfigToml>,
    /// GAP 13 + 14: AI-controller two-sink observability. When
    /// `enabled = true` AND `metadata_db_path` is set, the AI
    /// node carries its own [`crate::observability::ObservabilityContext`]
    /// so every mesh-internal `ai.chat` call records a
    /// metadata event + a provenance snapshot.
    #[serde(default)]
    pub two_sink: Option<TwoSinkConfig>,
}

/// `[observability.two_sink]` — Sink-A + Sink-B + Provenance
/// configuration for mesh-internal calls (the bridge already
/// carries its own ObservabilityContext on `AppState`; this
/// block is for the AI controller).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TwoSinkConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub metadata_db_path: Option<std::path::PathBuf>,
    /// When unset, defaults to the same directory as
    /// `metadata_db_path` with a `content.db` filename.
    #[serde(default)]
    pub content_db_path: Option<std::path::PathBuf>,
    /// When unset, defaults to the same directory as
    /// `metadata_db_path` with a `provenance.db` filename.
    #[serde(default)]
    pub provenance_db_path: Option<std::path::PathBuf>,
    #[serde(default = "default_content_retention_days")]
    pub content_retention_days: u32,
}

fn default_content_retention_days() -> u32 {
    7
}

/// `[observability.otel]` — operator-friendly shape that
/// projects into [`crate::observability::OtelConfig`]. We
/// avoid wiring the runtime struct directly so the operator
/// TOML stays minimal (no JSON whitelist for the attribute
/// keys, etc. — defaults apply).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct OtelConfigToml {
    #[serde(default)]
    pub enabled: bool,
    /// OTLP/HTTP traces endpoint (e.g.
    /// `http://localhost:4318/v1/traces`).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Resource service.name attribute. Defaults to
    /// `"relix-runtime"`.
    #[serde(default)]
    pub service_name: Option<String>,
    /// Event types operators want exported. When empty the
    /// exporter buffers nothing — fail-safe default.
    #[serde(default)]
    pub events: Vec<String>,
}

/// `[execution]` section — wraps `[[execution.agents]]` so
/// operators can extend the section with more execution-layer
/// switches in future waves (cost caps, retry policies, etc.)
/// without re-shaping the schema.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ExecutionSection {
    /// Per-agent access policies consumed by
    /// [`crate::nodes::execution::broker::AgentAccessBroker`].
    /// The broker keys off `policy.agent` so each entry's
    /// `agent` field must match the AIC's friendly name
    /// (`IdentityBundle::name`).
    #[serde(default)]
    pub agents: Vec<crate::nodes::execution::broker::AccessPolicy>,
    /// `[execution.gateway]` — GAP 11 three-tier transactional
    /// gateway. Absent means no persistent transaction store +
    /// no `execution.*` capabilities; the legacy in-memory
    /// `ActionGateway` keeps working as before.
    #[serde(default)]
    pub gateway: Option<GatewaySection>,
}

/// `[execution.gateway]` — GAP 11 settings.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct GatewaySection {
    /// When `true`, every dispatch through
    /// [`crate::nodes::tool::dispatcher::ToolDispatcher::dispatch_with_options`]
    /// returns a [`crate::nodes::execution::gateway_tier::DryRunPreview`]
    /// instead of invoking the handler. Mostly used for pre-
    /// production runs.
    #[serde(default)]
    pub dry_run: bool,
    /// Path to the transaction-store SQLite file. Required to
    /// boot the `execution.*` capabilities — when missing, the
    /// gateway runs in-memory only.
    #[serde(default)]
    pub db_path: Option<std::path::PathBuf>,
    /// Static Tier C list — tools that are NEVER permitted,
    /// regardless of caller.
    #[serde(default)]
    pub blocked_tools: Vec<String>,
    /// Optional evidence-store db path (GAP 12). When `Some`,
    /// the gateway captures one structured evidence record per
    /// dispatch. Defaults to the same DB as the transaction
    /// store when unset.
    #[serde(default)]
    pub evidence_db_path: Option<std::path::PathBuf>,
}

/// One peer alias.
#[derive(Clone, Debug, Deserialize)]
pub struct PeerConfig {
    /// libp2p TCP port to dial.
    pub port: u16,
}

/// SEC §16: `[knowledge_trust]` — receiver-side source-node binding
/// for knowledge sharing. Lists the identity public keys of trusted
/// peer nodes so `accept_shared` can bind a payload's signature to
/// the claimed `source_node`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct KnowledgeTrustConfig {
    /// Each trusted peer: its `source_node` name + identity key.
    #[serde(default)]
    pub source_nodes: Vec<TrustedSourceNode>,
    /// Explicit, logged opt-out: accept shares from a source with no
    /// configured key on signature ALONE (pre-§16 weak behaviour).
    /// Default `false` ⇒ such sources are rejected (fail closed).
    #[serde(default)]
    pub allow_unbound_sources: bool,
}

/// One trusted peer's identity, for knowledge-share source binding.
#[derive(Clone, Debug, Deserialize)]
pub struct TrustedSourceNode {
    /// The peer's `[controller] name` — the `source_node` it stamps
    /// on outbound `knowledge.accept_shared` payloads.
    pub node: String,
    /// The peer's identity Ed25519 PUBLIC key as 64-char lowercase
    /// hex (the verifying key of its `[identity] key_path`).
    pub pubkey: String,
}

impl KnowledgeTrustConfig {
    /// Parse each `pubkey` hex into 32 raw bytes, returning
    /// `(node, key)` pairs. Errors on malformed hex / wrong length.
    fn parsed_keys(&self) -> Result<Vec<(String, [u8; 32])>, String> {
        let mut out = Vec::with_capacity(self.source_nodes.len());
        for tsn in &self.source_nodes {
            let bytes = hex::decode(tsn.pubkey.trim())
                .map_err(|e| format!("source_node `{}`: pubkey not hex: {e}", tsn.node))?;
            if bytes.len() != 32 {
                return Err(format!(
                    "source_node `{}`: pubkey must be 32 bytes (64 hex chars), got {}",
                    tsn.node,
                    bytes.len()
                ));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            out.push((tsn.node.clone(), key));
        }
        Ok(out)
    }
}

/// SOL session declaration. Used by M6 once the SOL VM is wired.
#[derive(Clone, Debug, Deserialize)]
pub struct SessionConfig {
    /// Path to the `.sol` source.
    pub source: String,
}

/// What `relix-controller::main` calls. Returns when the runtime exits
/// (transport drops, or fatal error).
pub async fn run(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(config_path)?;
    let cfg: ControllerConfig = toml::from_str(&text)?;

    tracing::info!(
        node = %cfg.controller.name,
        node_type = %cfg.controller.node_type,
        port = cfg.controller.listen_port,
        "controller starting"
    );

    // Identity: load or generate the node's signing key.
    let node_signer = load_or_generate_key(&cfg.identity.key_path)?;
    let node_id = NodeId::from_pubkey(&node_signer.verifying_key().to_bytes());
    tracing::info!(node_id = %node_id, "node identity loaded");

    // Trust root: load org-root public key (32 raw bytes).
    let trust_root = load_pubkey(&cfg.trust.org_root_key_path)?;

    // Policy.
    let policy = if cfg.policy.file.exists() {
        PolicyEngine::from_path(&cfg.policy.file)?
    } else {
        tracing::warn!(
            policy_file = %cfg.policy.file.display(),
            "policy file missing — using permissive engine (alpha dev only)"
        );
        PolicyEngine::permissive()
    };
    if policy.is_permissive() {
        tracing::warn!("PERMISSIVE policy in effect — default-deny still applies per-method");
    }

    // Audit log (per node). Default `~/.relix/<node>/audit.log`.
    let data_dir = data_dir_for(&cfg.controller.name)?;
    std::fs::create_dir_all(&data_dir)?;
    let audit_path = data_dir.join("audit.log");

    // Manifest provider — populated as each node-type registers its
    // capabilities and served by the built-in `node.manifest` capability.
    //
    // SEC PART 2: thread the node's libp2p Ed25519 signing key
    // into the provider so `signed_snapshot` can mint the
    // wire-shaped [`SignedManifest`] receivers TOFU-pin +
    // verify. Pre-fix path returned an unsigned `NodeManifest`
    // — any peer on the transport could claim any capabilities.
    let manifest = ManifestProvider::new(
        node_id,
        cfg.controller.name.clone(),
        cfg.controller.node_type.clone(),
        NodeId::from_pubkey(trust_root.as_bytes()),
        vec![format!("/ip4/127.0.0.1/tcp/{}", cfg.controller.listen_port)],
    )
    .with_signer(node_signer.clone());

    // Dispatch bridge.
    let mut bridge = DispatchBridge::new(policy, trust_root, &audit_path, node_signer.clone())?;
    // GAP 23B: per-tenant policy resolver. When [policy] dir is
    // set, every admission step looks up
    // `{dir}/{tenant_id}.policy.toml` (sanitised) before falling
    // through to the global engine. The resolver is wired even
    // when `dir = None` so the `node.policy.tenant_*` caps can
    // answer with `count=0` rather than 404.
    {
        let global_handle = bridge.policy_handle();
        let resolver = std::sync::Arc::new(relix_core::policy::TenantPolicyResolver::new(
            global_handle,
            cfg.policy.dir.clone(),
            cfg.policy.tenant_cache_ttl_secs,
        ));
        if let Some(d) = cfg.policy.dir.as_ref() {
            tracing::info!(
                policy_dir = %d.display(),
                ttl_secs = cfg.policy.tenant_cache_ttl_secs,
                "per-tenant policy resolver wired"
            );
        }
        bridge.set_tenant_policy_resolver(resolver);
    }
    // GAP 23C: audit partition mirror. Wired only when
    // [audit] partition_by_tenant = true so existing
    // deployments stay byte-identical.
    if let Some(audit_cfg) = cfg.audit.as_ref()
        && audit_cfg.partition_by_tenant
    {
        let db_path = audit_cfg
            .db_path
            .clone()
            .unwrap_or_else(|| data_dir.join("audit-partition.db"));
        match crate::audit_partition::AuditPartitionStore::open(&db_path) {
            Ok(store) => {
                tracing::info!(
                    audit_partition_db = %db_path.display(),
                    "per-tenant audit partition mirror wired"
                );
                bridge.set_audit_partition_store(std::sync::Arc::new(store));
            }
            Err(e) => {
                tracing::warn!(
                    audit_partition_db = %db_path.display(),
                    error = %e,
                    "audit partition open failed; continuing without per-tenant mirror"
                );
            }
        }
    }
    // GAP 15 partial: always-require-approval allowlist. When
    // the operator listed any methods under
    // `[approval] always_require_methods`, register them with
    // the dispatch bridge so admission step 8.5 rejects every
    // dispatched call to those methods unless it carries an
    // approval token.
    if let Some(approval_cfg) = cfg.approval.as_ref()
        && !approval_cfg.always_require_methods.is_empty()
    {
        let methods = approval_cfg.always_require_methods.clone();
        tracing::info!(
            count = methods.len(),
            methods = ?methods,
            "approval: always-require allowlist wired"
        );
        bridge.set_always_require_methods(methods);
    }
    // P1: install the Ed25519 signing key for ApprovalTokens.
    // Sourced from `RELIX_APPROVAL_SIGNING_KEY` (64-hex-char
    // seed). Missing env var leaves the bridge in the no-key
    // configuration — every token-bearing call hits
    // `approval_token_missing_key`. We log loudly so operators
    // see the missing config at boot.
    match crate::approval::signer_from_env() {
        Ok(signer) => {
            tracing::info!(
                fingerprint = %signer.fingerprint(),
                "approval: Ed25519 signer loaded from {} (P1)",
                crate::approval::SIGNING_KEY_ENV
            );
            bridge.set_approval_signer(signer);
        }
        Err(_) => {
            tracing::warn!(
                env = crate::approval::SIGNING_KEY_ENV,
                "approval: signing key env var unset or malformed; ALL token-bearing \
                 admission calls will be denied with approval_token_missing_key. \
                 Set {} to a 64-hex-char Ed25519 seed to enable structured tokens.",
                crate::approval::SIGNING_KEY_ENV
            );
        }
    }
    // W2: build the per-controller access broker from
    // `[[execution.agents]]`. Absent / empty config produces
    // an empty broker — every check returns Allow so
    // existing deployments behave identically. The same Arc
    // is shared with the ToolDispatcher (built later in the
    // AI registration path) so both the dispatch admission
    // pipeline and the ToolDispatcher see one source of
    // truth.
    let access_broker = build_access_broker(&cfg);
    bridge.set_access_broker(access_broker.clone());

    // RELIX-7.11: build the metrics collector + spawn the drain
    // + retention loops. Wires the sink onto the dispatch bridge
    // so every dispatched call writes one row. When
    // `[metrics] enabled = false` (or the section is absent),
    // the bridge stays sink-less and the existing dispatch-stats
    // counters remain the only data surface.
    let metrics_bundle = build_metrics_bundle(&cfg, &data_dir)?;
    if let Some(b) = metrics_bundle.as_ref() {
        bridge.set_metrics_sink(b.sink.clone(), cfg.controller.name.clone());
    }
    // RELIX-7.28 Part 1: budget enforcer. Built on top of the
    // metrics query engine so its in-memory cache can refresh
    // accumulated spend from the metrics SQLite store. When
    // [budget] is absent / inactive (no caps configured), the
    // bundle returns None and the dispatch path stays pre-7.28.
    let budget_bundle = build_budget_bundle(&cfg, metrics_bundle.as_ref());
    if let Some(b) = budget_bundle.as_ref() {
        bridge.set_budget_enforcer(b.enforcer.clone());
        // Wire the same alert pipeline used by the §7.19 alerts
        // so BudgetExceeded events ride through the chronicle +
        // multi-channel fan-out exactly like other alerts.
        if let Some(m) = metrics_bundle.as_ref() {
            let chronicle = crate::metrics::ChronicleAlertSink::new(m.alert_chronicle.clone());
            let multi = crate::metrics::MultiChannelAlertSink::new(
                m.alert_mesh_cell.clone(),
                m.alert_targets.clone(),
            );
            let composite = crate::metrics::CompositeAlertSink::new(vec![
                std::sync::Arc::new(chronicle),
                std::sync::Arc::new(multi),
            ]);
            b.enforcer.set_alert_sink(std::sync::Arc::new(composite));
            // Force-invalidate the cache on every cost-bearing
            // row so a single expensive call can't escape a cap
            // by being the last call before the next check.
            m.collector.set_budget_enforcer(b.enforcer.clone());
        }
    }
    // RELIX-7.28 Part 3: mesh PII gate. Built independently of
    // the metrics bundle — the gate has its own SQLite chronicle
    // (or shares the metrics dir). When [mesh_pii] is absent /
    // disabled, the bundle returns None and the dispatch path
    // skips the gate entirely.
    let pii_gate_bundle = build_pii_gate_bundle(&cfg, &data_dir)?;
    if let Some(gate) = pii_gate_bundle.as_ref() {
        bridge.set_pii_gate(gate.clone());
    }
    // RELIX-7.15: training-data pipeline. Returns Ok(None) when
    // the `[training]` section is absent / disabled — the AI
    // handler then runs with `interaction_sink = None` and
    // every other code path stays untouched.
    let training_bundle = build_training_bundle(&cfg, &data_dir)?;

    // RELIX-7.19 GAP 4: confidence scoring + fallback. Returns
    // Ok(None) when [confidence] is absent or enabled = false.
    // When present, wires scorer + fallback engine + shared
    // last-confidence cell into the dispatch bridge, AND
    // registers the bridge's `Alert` fallback action with the
    // alert pipeline from `[metrics.alerts]` so LowConfidence
    // events fan out through MultiChannelAlertSink +
    // ChronicleAlertSink.
    let confidence_bundle = build_confidence_bundle(&cfg)?;
    if let Some(b) = confidence_bundle.as_ref() {
        bridge.set_confidence(b.scorer.clone(), b.engine.clone());
        bridge.set_last_confidence_cell(b.cell.clone());
        // Wire the alert pipeline when [metrics.alerts] is
        // also configured. The composite sink writes to the
        // chronicle synchronously AND fans out to channel
        // targets non-blocking via MultiChannelAlertSink.
        if let Some(m) = metrics_bundle.as_ref() {
            let chronicle = crate::metrics::ChronicleAlertSink::new(m.alert_chronicle.clone());
            let multi = crate::metrics::MultiChannelAlertSink::new(
                m.alert_mesh_cell.clone(),
                m.alert_targets.clone(),
            );
            let composite = crate::metrics::CompositeAlertSink::new(vec![
                std::sync::Arc::new(chronicle),
                std::sync::Arc::new(multi),
            ]);
            let sink: std::sync::Arc<dyn crate::metrics::AlertDeliver> =
                std::sync::Arc::new(composite);
            let engine = std::sync::Arc::new(m.alert_engine.clone());
            bridge.set_alert_pipeline(engine, sink.clone());
            // PART 3: install the same alert sink + price table
            // on the SC stats so the cost guards can emit
            // CostAlerts when the trigger-rate / hourly-budget
            // limits trip, and so per-request cost estimation
            // is grounded in real model prices.
            b.sc_stats.install_alert_sink(sink.clone());
            b.sc_stats.install_price_table(m.collector.prices());
            // PART 4: install the absolute spend caps on the
            // metrics collector. Every cost-bearing
            // record_invocation now checks the rolling hourly,
            // daily, and per-request caps and emits a CostAlert
            // on overshoot.
            m.collector
                .install_absolute_caps(m.cost_alerts_cfg.clone(), sink);
        }
        // Register confidence.* coordinator caps so operators
        // can call confidence.policy_list / score_history /
        // reset_history through the dispatch bridge.
        crate::confidence::register(
            &mut bridge,
            b.scorer.clone(),
            b.engine.clone(),
            b.sc_stats.clone(),
            b.sc_cfg.clone(),
        );
        for (method, doc) in crate::confidence::confidence_capability_descriptors() {
            manifest.add_capability(
                relix_core::capability::CapabilityDescriptor::unary(*method)
                    .with_description(*doc)
                    .with_categories(
                        if method.starts_with("confidence.reset") {
                            ["mutate", "confidence"]
                        } else {
                            ["read", "confidence"]
                        }
                        .iter()
                        .map(|s| (*s).into()),
                    ),
            );
        }
    }

    register_builtins(&mut bridge, &cfg, manifest.clone());
    // Router role short-circuits: it doesn't run the per-node-type
    // capability surface (memory/ai/tool/...) — it runs the four
    // router.* capabilities and the reaper background loops.
    // Post-startup wiring registry. Each entry is one
    // dispatcher cell + config the run() loop must populate
    // AFTER the rpc::Client + bridge are up. A single node
    // (e.g. the coordinator) may register multiple hooks
    // here — drift embedder AND workflow dispatcher both
    // wire late from the same boot.
    let mut startup_wiring: Vec<StartupWiring> = Vec::new();
    let router_state = if cfg.controller.role == "router" {
        tracing::info!(
            role = "router",
            session_ttl_secs = cfg.controller.session_ttl_secs,
            "starting controller with role: router"
        );
        let state = std::sync::Arc::new(std::sync::Mutex::new(
            crate::nodes::router::RouterState::new(
                node_id.to_string(),
                cfg.controller.name.clone(),
                cfg.controller.session_ttl_secs,
            ),
        ));
        crate::nodes::router::register(&mut bridge, state.clone());
        register_router_descriptors(&manifest);
        Some(state)
    } else {
        register_node_type_handlers(
            &mut bridge,
            &cfg,
            manifest.clone(),
            access_broker.clone(),
            &mut startup_wiring,
            metrics_bundle.as_ref(),
            training_bundle.as_ref(),
            budget_bundle.as_ref(),
            pii_gate_bundle.as_ref(),
            confidence_bundle.as_ref(),
        )?;
        None
    };

    let bridge = Arc::new(bridge);

    // Transport.
    let (client, mut events, event_loop) =
        rpc::new(node_signer.to_bytes(), cfg.controller.listen_port).await?;
    tokio::spawn(event_loop.run());

    // RELIX-2 step 2: spawn the streaming-substream accept
    // task. Every controller registers the `/relix/rpc/stream/1`
    // protocol — registration is idempotent at the
    // libp2p_stream behaviour layer and zero-cost when no
    // streaming handlers are wired (no incoming substream
    // ever arrives). When a peer opens a streaming substream,
    // the task pulls the inbound request envelope (which the
    // caller wrote at the head of the stream) and routes it
    // through `bridge.handle_inbound_stream`, where the full
    // admission pipeline runs. A failed `accept_streams` is
    // logged at warn level and the task exits; subsequent
    // controllers in the same process would surface the same
    // failure (the protocol is registered once per
    // libp2p_stream::Behaviour instance).
    match client.accept_streams() {
        Ok(mut incoming) => {
            let bridge_for_streams = bridge.clone();
            tokio::spawn(async move {
                use crate::transport::stream::StreamWriter;
                use futures::StreamExt;
                while let Some((peer, raw_stream)) = incoming.next().await {
                    let bridge = bridge_for_streams.clone();
                    tokio::spawn(async move {
                        let mut writer = StreamWriter::new(raw_stream);
                        let envelope = match writer.read_request_envelope().await {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    peer = %peer,
                                    "streaming: caller closed substream before envelope arrived"
                                );
                                return;
                            }
                        };
                        bridge.handle_inbound_stream(envelope, writer).await;
                    });
                }
            });
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "controller: failed to register streaming protocol; streaming capabilities disabled"
            );
        }
    }

    // Dial configured peers.
    for (alias, peer_cfg) in &cfg.peers {
        let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{}", peer_cfg.port).parse()?;
        match client.dial(addr.clone()).await {
            Ok(()) => tracing::info!(alias = %alias, addr = %addr, "dialed peer"),
            Err(e) => {
                tracing::warn!(alias = %alias, addr = %addr, error = %e, "dial failed (will retry on demand)")
            }
        }
    }
    client.bootstrap_kademlia().await;

    // W7: optional OTel exporter loop. When the controller's
    // `[observability.otel]` block is enabled + has an
    // endpoint, build the exporter once and spawn a tokio task
    // that calls `flush()` every 5 seconds. The exporter is
    // currently producer-less on a controller (controllers
    // don't run a bridge-style ObservabilityContext), so the
    // flush loop is forward-compat plumbing — when controllers
    // gain their own metadata sink the same exporter
    // instance becomes the OTLP shipping path.
    if let Some(otel_cfg) = build_otel_config(&cfg) {
        let exporter = std::sync::Arc::new(crate::observability::OtelExporter::new(otel_cfg));
        // The spawned task owns its own clone — that clone
        // keeps the exporter alive as long as the loop runs
        // (forever, in practice), so the original Arc going
        // out of scope here is harmless.
        let exp_clone = exporter.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                let _ = exp_clone.flush().await;
            }
        });
        tracing::info!("observability: spawned OTLP exporter flush loop (interval=5s)");
    }

    // Router-only background loops.
    if let Some(state) = router_state.clone() {
        let stale = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.tick().await; // skip first immediate tick
            loop {
                tick.tick().await;
                if let Ok(mut g) = stale.lock() {
                    g.reap_stale_peers();
                }
            }
        });
        let sessions = state;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.tick().await; // skip first immediate tick
            loop {
                tick.tick().await;
                if let Ok(mut g) = sessions.lock() {
                    g.reap_expired_sessions();
                }
            }
        });
        tracing::info!("router: spawned stale-peer reaper (30s) + session reaper (300s) loops");
    } else {
        // Controller role: optional heartbeat sender to the
        // designated router peer. Non-fatal — the controller
        // still boots when the router is down or unconfigured.
        if let Some(router_peer_str) = cfg
            .controller
            .router_peer_id
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            match router_peer_str.parse::<rpc::PeerId>() {
                Ok(router_peer) => {
                    spawn_heartbeat_sender(
                        client.clone(),
                        router_peer,
                        cfg.controller.name.clone(),
                        client.peer_id().to_string(),
                        manifest.clone(),
                        cfg.identity.key_path.clone(),
                    );
                    tracing::info!(
                        router = %router_peer_str,
                        "controller: heartbeat sender scheduled (1.5s warmup, then every 60s)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        router_peer_id = %router_peer_str,
                        error = %e,
                        "controller: router_peer_id is not a valid libp2p PeerId; heartbeat sender disabled"
                    );
                }
            }
        } else {
            tracing::info!("controller: no router_peer_id configured; heartbeat sender disabled");
        }
    }

    // Post-startup wiring. Each entry registered by
    // `register_node_type_handlers` spawns its own
    // discovery / dial / cell-populate background task here.
    // Failures inside any one task are non-fatal — the
    // capability the cell backs simply stays in its
    // "not-yet-wired" state.
    for wiring in startup_wiring.drain(..) {
        match wiring {
            StartupWiring::AiMemory {
                cell,
                cfg: Some(memcfg),
            } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_ai_memory_cell(cell, memcfg, key_path).await;
                });
            }
            StartupWiring::AiMemory { cfg: None, .. } => {}
            StartupWiring::MemoryCurator {
                ai_cell,
                coord_cell,
                state,
                cfg: ccfg,
                embedding_cell,
                embedding_cfg,
            } => {
                let interval_secs = ccfg.interval_secs;
                if let Some(aipeer) = ccfg.ai_peer.clone() {
                    let key_path = cfg.identity.key_path.clone();
                    let state_for_ai = state.clone();
                    tokio::spawn(async move {
                        populate_memory_curator_cell(
                            ai_cell,
                            state_for_ai,
                            aipeer,
                            key_path,
                            interval_secs,
                        )
                        .await;
                    });
                } else {
                    tracing::info!(
                        "memory curator: no [memory.curator.ai_peer]; AI dispatcher unset"
                    );
                }
                if let Some(coordpeer) = ccfg.coord_peer.clone() {
                    let key_path = cfg.identity.key_path.clone();
                    tokio::spawn(async move {
                        populate_memory_curator_coord_cell(coord_cell, coordpeer, key_path).await;
                    });
                } else {
                    tracing::info!(
                        "memory curator: no [memory.curator.coord_peer]; chronicle events disabled"
                    );
                }
                if let (Some(cell), Some(epeer)) = (embedding_cell, embedding_cfg) {
                    let key_path = cfg.identity.key_path.clone();
                    tokio::spawn(async move {
                        populate_memory_embedding_cell(cell, epeer, key_path).await;
                    });
                }
                let _ = state;
            }
            StartupWiring::MemoryEmbedding { cell, cfg: epeer } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_memory_embedding_cell(cell, epeer, key_path).await;
                });
            }
            StartupWiring::Telegram { cell, cfg: tg_cfg } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_telegram_outbound_cell(cell, tg_cfg, key_path).await;
                });
            }
            StartupWiring::Discord { cell, cfg: dc_cfg } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_discord_outbound_cell(cell, dc_cfg, key_path).await;
                });
            }
            StartupWiring::Slack { cell, cfg: sl_cfg } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_slack_outbound_cell(cell, sl_cfg, key_path).await;
                });
            }
            StartupWiring::Email { cell, cfg: em_cfg } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_email_outbound_cell(cell, *em_cfg, key_path).await;
                });
            }
            StartupWiring::CoordDriftEmbed { cell, cfg: ai_cfg } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_drift_embedder_cell(cell, ai_cfg, key_path).await;
                });
            }
            StartupWiring::CoordWorkflowDispatcher {
                cell,
                peers,
                deadline_secs,
            } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_workflow_dispatcher_cell(cell, peers, key_path, deadline_secs).await;
                });
            }
            StartupWiring::CoordAlertMesh {
                cell,
                peers,
                deadline_secs,
            } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_alert_mesh_cell(cell, peers, key_path, deadline_secs).await;
                });
            }
            StartupWiring::KnowledgeMesh {
                cell,
                peers,
                deadline_secs,
                source_key_registry,
            } => {
                let key_path = cfg.identity.key_path.clone();
                tokio::spawn(async move {
                    populate_knowledge_mesh_cell(
                        cell,
                        peers,
                        key_path,
                        deadline_secs,
                        source_key_registry,
                    )
                    .await;
                });
            }
        }
    }

    tracing::info!("controller online; awaiting inbound RPCs");

    // SEC PART 1: build a peer_id → alias map so the agent
    // gate's surface_allowlist check can match against a
    // transport-derived surface rather than the operator-
    // asserted envelope.surface. The map is populated as
    // PeerConnected events arrive — we match the reported
    // multiaddr's `/ip4/.../tcp/<port>` substring against the
    // peers config, then stash (peer_id → alias).
    let peer_alias_map: std::sync::Arc<
        std::sync::RwLock<std::collections::HashMap<libp2p::PeerId, String>>,
    > = std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let alias_by_port: std::collections::HashMap<u16, String> = cfg
        .peers
        .iter()
        .map(|(alias, peer_cfg)| (peer_cfg.port, alias.clone()))
        .collect();

    // Inbound event loop.
    let bridge_for_loop = bridge.clone();
    while let Some(event) = events.recv().await {
        match event {
            TransportEvent::Request {
                envelope,
                from,
                respond,
            } => {
                let bridge = bridge_for_loop.clone();
                let alias_map = peer_alias_map.clone();
                tokio::spawn(async move {
                    // SEC PART 1: derive the trusted caller
                    // surface from the libp2p PeerId. `None`
                    // when the peer hasn't yet been mapped —
                    // the gate then treats this as "unknown
                    // surface" (denied if surface_allowlist is
                    // non-empty).
                    let caller_surface = alias_map.read().ok().and_then(|g| g.get(&from).cloned());
                    let resp = bridge
                        .handle_inbound_with_surface(envelope, caller_surface)
                        .await;
                    respond.respond(resp).await;
                });
            }
            TransportEvent::PeerConnected { peer_id, address } => {
                // SEC PART 1: stash peer_id → alias for the
                // surface derivation above. We match the
                // reported address's `/tcp/<port>` segment
                // against `alias_by_port`. Aliases unknown to
                // the local peers config simply stay absent —
                // their requests get caller_surface = None.
                let reported = address.to_string();
                let alias = alias_by_port.iter().find_map(|(port, alias)| {
                    let needle = format!("/tcp/{port}");
                    if reported.contains(&needle) {
                        Some(alias.clone())
                    } else {
                        None
                    }
                });
                if let Some(alias) = alias.clone()
                    && let Ok(mut g) = peer_alias_map.write()
                {
                    g.insert(peer_id, alias);
                }
                tracing::info!(
                    peer = %peer_id,
                    addr = %address,
                    alias = ?alias,
                    "peer connected"
                );
            }
            TransportEvent::PeerDisconnected { peer_id } => {
                // SEC §18: drop the peer_id → alias surface mapping so a
                // departed peer's PeerId can't carry a stale caller
                // surface if the id is later reused. (Knowledge-share
                // source keys are removed by the knowledge mesh's own
                // disconnect consumer over its discovery transport.)
                if let Ok(mut g) = peer_alias_map.write() {
                    g.remove(&peer_id);
                }
                tracing::info!(peer = %peer_id, "peer disconnected");
            }
        }
    }

    Ok(())
}

/// Payload returned by `node.health` — a multi-line `key=value\n` string.
///
/// SIMP-016: alpha capabilities take and return strings only. The plain-text
/// format keeps the response readable both for `relix-cli ping` (which prints
/// it verbatim) and for SOL flows (`let h: str = remote_call("controller",
/// "node.health", ""); print(h);`). Typed CBOR payloads land at Gate 2 with
/// the CDDL stdlib.
fn node_health_body(cfg: &ControllerConfig) -> String {
    format!(
        "name={}\ntype={}\nstatus=ok\nruntime={}\n",
        cfg.controller.name,
        cfg.controller.node_type,
        env!("CARGO_PKG_VERSION"),
    )
}

/// Register capabilities every node serves by default.
///
/// `node.health` returns a multi-line `key=value` string (operator-readable).
/// `node.manifest` returns the current capability snapshot as CBOR-encoded
/// [`crate::manifest::NodeManifest`] — that's the M10 discovery primitive.
fn register_builtins(
    bridge: &mut DispatchBridge,
    cfg: &ControllerConfig,
    manifest: ManifestProvider,
) {
    let body = node_health_body(cfg);
    bridge.register(
        "node.health",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let body = body.clone();
            async move { HandlerOutcome::Ok(body.into_bytes()) }
        })),
    );
    // W2-006b: node.dispatch.stats — per-capability latency +
    // outcome counters from the local DispatchBridge. Pure
    // read; never gates a decision. The handler captures an
    // Arc clone of the stats lock so it doesn't need the
    // bridge.
    let stats_handle = bridge.capability_stats_handle();
    bridge.register(
        "node.dispatch.stats",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let stats = stats_handle.clone();
            async move {
                let body = dispatch_stats_body(&stats);
                HandlerOutcome::Ok(body.into_bytes())
            }
        })),
    );
    // W2-007a: node.policy.simulate — answer "what would the
    // policy say if caller X (groups=Y,Z) tried method M?"
    // without actually invoking M. Pure read. Helps operators
    // validate policy changes before deploying them.
    let policy_handle = bridge.policy_handle();
    bridge.register(
        "node.policy.simulate",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let policy = policy_handle.clone();
            async move { handle_policy_simulate(&policy, &ctx) }
        })),
    );
    // W2-007d: node.policy.recent_denials — bounded ring of
    // recent policy-denied attempts on the local dispatch
    // bridge. Pure read. Lets operators see who tried what
    // that we refused without trawling the audit log.
    let denial_ring = bridge.policy_denials_handle();
    bridge.register(
        "node.policy.recent_denials",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let ring = denial_ring.clone();
            async move { handle_policy_recent_denials(&ring, &ctx) }
        })),
    );
    // GAP 23C: node.audit.tenant_list + node.audit.tenant_recent.
    // Pure read of the audit partition mirror. Both no-op
    // (count=0 / error) when partitioning is disabled on this
    // node — keeps existing deployments backwards-compatible.
    let audit_part_for_list = bridge.audit_partition_handle();
    bridge.register(
        "node.audit.tenant_list",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let p = audit_part_for_list.clone();
            async move { handle_audit_tenant_list(p.as_deref(), &ctx) }
        })),
    );
    let audit_part_for_recent = bridge.audit_partition_handle();
    bridge.register(
        "node.audit.tenant_recent",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let p = audit_part_for_recent.clone();
            async move { handle_audit_tenant_recent(p.as_deref(), &ctx) }
        })),
    );
    // GAP 23B: node.policy.tenant_list + node.policy.tenant_get.
    // Pure read of the per-tenant policy directory. Both no-op
    // (404-style) when the bridge wasn't configured with a
    // tenant resolver — keeps single-tenant deployments
    // backwards-compatible.
    let tenant_resolver_for_list = bridge.tenant_policy_handle();
    bridge.register(
        "node.policy.tenant_list",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let r = tenant_resolver_for_list.clone();
            async move { handle_policy_tenant_list(r.as_deref(), &ctx) }
        })),
    );
    let tenant_resolver_for_get = bridge.tenant_policy_handle();
    bridge.register(
        "node.policy.tenant_get",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let r = tenant_resolver_for_get.clone();
            async move { handle_policy_tenant_get(r.as_deref(), &ctx) }
        })),
    );
    // Built-in: every node serves its own SignedManifest.
    //
    // SEC PART 2: the wire response is a signed envelope —
    // signer is the node's libp2p Ed25519 key (installed at
    // ManifestProvider construction above). Receivers TOFU-pin
    // the fingerprint and verify the signature; the
    // freshness check (`signed_at_ms` vs configured ttl_secs)
    // fires `MANIFEST_STALE` on the receiver side.
    let manifest_for_handler = manifest.clone();
    bridge.register(
        "node.manifest",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let provider = manifest_for_handler.clone();
            async move {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                    .unwrap_or(0);
                let signed = match provider.signed_snapshot(now_ms) {
                    Ok(s) => s,
                    Err(e) => {
                        return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                            cause: format!("node.manifest sign: {e}"),
                            retry_hint: 1,
                            retry_after: None,
                        });
                    }
                };
                match codec::encode(&signed) {
                    Ok(bytes) => HandlerOutcome::Ok(bytes),
                    Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                        cause: format!("node.manifest encode: {e}"),
                        retry_hint: 1,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
    // Advertise the built-ins themselves.
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.health")
            .with_description("Liveness probe. Returns 'ok' if the node is up.")
            .with_categories(["health".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.manifest")
            .with_description("Return this node's manifest (capability list + node identity).")
            .with_categories(["discover".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
    // W2-006b: dispatch stats descriptor.
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.dispatch.stats")
            .with_description(
                "Per-capability invocation counters + latency stats from the local DispatchBridge. \
                 Tab-delim rows: method\\tinvocations\\terrors\\tdenied\\tunknown_method\\tlast_invoked_at\\tlast_error_at\\tlatency_samples\\tlast_elapsed_ms\\tmax_elapsed_ms\\tmean_elapsed_ms — followed by `count=N`.",
            )
            .with_categories(["observe".into(), "read".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
    // W2-007a: policy simulate descriptor.
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.policy.simulate")
            .with_description(
                "Evaluate the local policy against a hypothetical caller (groups) + method tuple. \
                 Arg shape: `<method>|<comma-separated-groups>`. Returns multi-line key=value: \
                 `decision=allow|deny\\nmatched_rule=<rule_or_->\\nreason=<reason_or_->`. \
                 Pure read; never invokes the method. Useful for validating policy changes \
                 before deploying them.",
            )
            .with_categories(["observe".into(), "policy".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
    // W2-007d: recent denials descriptor.
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.policy.recent_denials")
            .with_description(
                "Bounded ring of recent policy-denied attempts (capacity 256, newest first). \
                 Optional arg: max row count as a positive integer. \
                 Returns tab-delim rows: at\\tmethod\\tcaller_subject_id\\tcaller_name\\trule\\treason, \
                 followed by `count=N`. Resets on bridge restart.",
            )
            .with_categories(["observe".into(), "policy".into(), "read".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
    // GAP 23B: per-tenant policy enumeration + inspection.
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.policy.tenant_list")
            .with_description(
                "Enumerate tenant ids that have a per-tenant policy file at \
                 `{policy.dir}/{tenant_id}.policy.toml`. Empty arg. Returns one tenant id \
                 per line followed by `count=N`. Returns `count=0` when [policy] dir is \
                 unset or the directory is empty.",
            )
            .with_categories(["observe".into(), "policy".into(), "read".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.policy.tenant_get")
            .with_description(
                "Read the raw TOML text of `{policy.dir}/{tenant_id}.policy.toml`. \
                 Arg: tenant id as a UTF-8 string. Returns the TOML text on hit, \
                 a NOT_FOUND error envelope on miss.",
            )
            .with_categories(["observe".into(), "policy".into(), "read".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
    // GAP 23C: per-tenant audit enumeration + inspection.
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.audit.tenant_list")
            .with_description(
                "Enumerate every distinct tenant id seen by the audit partition mirror. \
                 Empty arg. Returns one tenant id per line followed by `count=N`. \
                 Returns `count=0` when partitioning is disabled or no traffic has flowed yet.",
            )
            .with_categories(["observe".into(), "audit".into(), "read".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.audit.tenant_recent")
            .with_description(
                "Return the most recent audit rows for a tenant from the partition mirror. \
                 Arg: `<tenant_id>|<limit>` (limit optional; defaults 100, clamped to [1,1000]). \
                 Returns JSON: {\"tenant_id\": ..., \"count\": N, \"rows\": [...]}. \
                 Newest first.",
            )
            .with_categories(["observe".into(), "audit".into(), "read".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe),
    );
}

/// W2-007d: handle `node.policy.recent_denials`. Optional arg
/// is a positive integer max row count (default 100,
/// server-capped at 500). Emits one tab-delim line per entry
/// newest-first, plus trailing `count=N`.
fn handle_policy_recent_denials(
    ring: &std::sync::Arc<crate::dispatch::PolicyDenialRing>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    use relix_core::types::error_kinds;
    use std::fmt::Write as _;
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => {
            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("node.policy.recent_denials arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let max = if s.is_empty() {
        100
    } else {
        match s.parse::<usize>() {
            Ok(v) if v > 0 => v.min(500),
            _ => {
                return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                    kind: error_kinds::INVALID_ARGS,
                    cause: format!(
                        "node.policy.recent_denials: arg must be a positive integer (got '{s}')"
                    ),
                    retry_hint: 2,
                    retry_after: None,
                });
            }
        }
    };
    let rows = ring.snapshot_newest_first(max);
    let count = rows.len();
    let mut body = String::new();
    for r in &rows {
        // Strip tabs from free-form fields so the row format
        // stays grep-friendly. The audit log keeps the
        // canonical values.
        let safe_reason = r.reason.replace(['\t', '\n'], " ");
        let safe_name = r.caller_name.replace(['\t', '\n'], " ");
        let _ = writeln!(
            body,
            "{}\t{}\t{}\t{}\t{}\t{}",
            r.at, r.method, r.caller_subject_id, safe_name, r.rule, safe_reason
        );
    }
    let _ = writeln!(body, "count={count}");
    HandlerOutcome::Ok(body.into_bytes())
}

/// GAP 23B: handle `node.policy.tenant_list`. Pure read of the
/// configured per-tenant policy directory. When the bridge was
/// built without a resolver (single-tenant mode) the cap returns
/// `count=0` rather than an error, so dashboards can poll it
/// uniformly.
fn handle_policy_tenant_list(
    resolver: Option<&relix_core::policy::TenantPolicyResolver>,
    _ctx: &InvocationCtx,
) -> HandlerOutcome {
    use std::fmt::Write as _;
    let tenants: Vec<String> = match resolver {
        Some(r) => r.list_tenants(),
        None => Vec::new(),
    };
    let count = tenants.len();
    let mut body = String::new();
    for t in &tenants {
        let _ = writeln!(body, "{t}");
    }
    let _ = writeln!(body, "count={count}");
    HandlerOutcome::Ok(body.into_bytes())
}

/// GAP 23B: handle `node.policy.tenant_get`. Arg is the tenant
/// id as a UTF-8 string. Returns the raw TOML on hit;
/// `UNKNOWN_METHOD` (re-purposed as "no such resource") on miss.
fn handle_policy_tenant_get(
    resolver: Option<&relix_core::policy::TenantPolicyResolver>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    use relix_core::types::error_kinds;
    let tenant = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => {
            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("node.policy.tenant_get arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if tenant.is_empty() {
        return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "node.policy.tenant_get: tenant id required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let resolver = match resolver {
        Some(r) => r,
        None => {
            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                kind: error_kinds::UNKNOWN_METHOD,
                cause: "node.policy.tenant_get: per-tenant policy not configured".into(),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    match resolver.tenant_policy_text(tenant) {
        Some(text) => HandlerOutcome::Ok(text.into_bytes()),
        None => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
            kind: error_kinds::UNKNOWN_METHOD,
            cause: format!("node.policy.tenant_get: no policy file for tenant {tenant:?}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

/// GAP 23C: handle `node.audit.tenant_list`. Pure read of the
/// partition mirror's distinct tenant ids. Returns `count=0`
/// when partitioning is disabled — dashboards poll uniformly.
fn handle_audit_tenant_list(
    store: Option<&crate::audit_partition::AuditPartitionStore>,
    _ctx: &InvocationCtx,
) -> HandlerOutcome {
    use std::fmt::Write as _;
    let tenants: Vec<String> = match store {
        Some(s) => match s.list_tenants() {
            Ok(v) => v,
            Err(e) => {
                return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                    kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                    cause: format!("audit partition list_tenants: {e}"),
                    retry_hint: 1,
                    retry_after: None,
                });
            }
        },
        None => Vec::new(),
    };
    let count = tenants.len();
    let mut body = String::new();
    for t in &tenants {
        let _ = writeln!(body, "{t}");
    }
    let _ = writeln!(body, "count={count}");
    HandlerOutcome::Ok(body.into_bytes())
}

/// GAP 23C: handle `node.audit.tenant_recent`. Arg shape
/// `<tenant_id>|<limit>` (limit optional, default 100).
/// Returns JSON `{ "tenant_id": ..., "count": N, "rows": [...] }`.
fn handle_audit_tenant_recent(
    store: Option<&crate::audit_partition::AuditPartitionStore>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    use relix_core::types::error_kinds;
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => {
            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("node.audit.tenant_recent arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if raw.is_empty() {
        return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "node.audit.tenant_recent: tenant id required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let (tenant, limit) = match raw.split_once('|') {
        Some((t, l)) => {
            let n = l
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .unwrap_or(100);
            (t.trim(), n)
        }
        None => (raw, 100usize),
    };
    if tenant.is_empty() {
        return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "node.audit.tenant_recent: tenant id required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let store = match store {
        Some(s) => s,
        None => {
            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                kind: error_kinds::UNKNOWN_METHOD,
                cause: "node.audit.tenant_recent: audit partition not configured".into(),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let rows = match store.tenant_recent(tenant, limit) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("audit partition tenant_recent: {e}"),
                retry_hint: 1,
                retry_after: None,
            });
        }
    };
    let body = serde_json::json!({
        "tenant_id": tenant,
        "count": rows.len(),
        "rows": rows,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("node.audit.tenant_recent encode: {e}"),
            retry_hint: 1,
            retry_after: None,
        }),
    }
}

/// W2-007a: handle `node.policy.simulate`. Parses `<method>|<groups_csv>`,
/// builds a synthetic VerifiedIdentity with the supplied groups,
/// runs PolicyEngine::evaluate, and returns the Decision as
/// multi-line key=value. The synthetic identity carries the
/// CALLER's identity for subject_id / name (so the simulation
/// inherits the caller's identity but with a hypothetical
/// groups list).
fn handle_policy_simulate(policy: &PolicyEngine, ctx: &InvocationCtx) -> HandlerOutcome {
    use relix_core::identity::VerifiedIdentity;
    use relix_core::policy::Decision;
    use relix_core::types::error_kinds;

    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => {
            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("node.policy.simulate arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let (method, groups_csv) = match s.split_once('|') {
        Some(p) => p,
        None => {
            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: "node.policy.simulate: arg shape `<method>|<groups_csv>`".into(),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let method = method.trim();
    if method.is_empty() {
        return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "node.policy.simulate: method required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let groups: Vec<String> = groups_csv
        .split(',')
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .collect();
    // Build a hypothetical identity that inherits the caller's
    // subject_id / org_id (so audit-style admin tooling that
    // distinguishes "who's asking" still works) but swaps the
    // groups for the simulated set. Name is suffixed with
    // `:simulate` so log lines + audit trails know the
    // evaluation was hypothetical.
    let hypothetical = VerifiedIdentity {
        subject_id: ctx.caller.subject_id,
        name: format!("{}:simulate", ctx.caller.name),
        org_id: ctx.caller.org_id,
        groups,
        role: ctx.caller.role.clone(),
        clearance: ctx.caller.clearance.clone(),
        bundle_id: ctx.caller.bundle_id,
    };
    let decision = policy.evaluate(&hypothetical, method);
    use std::fmt::Write as _;
    let mut body = String::new();
    match &decision {
        Decision::Allow { matched_rule } => {
            let _ = writeln!(body, "decision=allow");
            let _ = writeln!(body, "matched_rule={}", matched_rule);
            let _ = writeln!(body, "reason=-");
        }
        Decision::Deny {
            reason,
            matched_rule,
        } => {
            let _ = writeln!(body, "decision=deny");
            let _ = writeln!(
                body,
                "matched_rule={}",
                matched_rule.as_deref().unwrap_or("-")
            );
            let _ = writeln!(body, "reason={}", reason);
        }
    }
    HandlerOutcome::Ok(body.into_bytes())
}

/// W2-006b: format the dispatch-stats snapshot as tab-delim
/// rows. The output mirrors the row schema described in the
/// `node.dispatch.stats` capability descriptor. Mean elapsed
/// is `total / samples` when samples > 0; otherwise 0.
fn dispatch_stats_body(
    stats: &std::sync::RwLock<std::collections::HashMap<String, crate::dispatch::CapStats>>,
) -> String {
    use std::fmt::Write as _;
    let snap: Vec<(String, crate::dispatch::CapStats)> = {
        let g = stats.read().expect("capability_stats read");
        g.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    // Stable ordering — lexicographic by method name — so
    // operators diff cleanly across calls.
    let mut snap = snap;
    snap.sort_by(|a, b| a.0.cmp(&b.0));
    let mut body = String::new();
    for (name, s) in &snap {
        let mean = s
            .total_elapsed_ms
            .checked_div(s.latency_samples)
            .unwrap_or(0);
        // W2-006d: 12th column is the recent-latencies ring
        // as comma-separated u32s, oldest-first. `-` when
        // empty so the column always has a parse target.
        let samples_csv = if s.recent_latencies.is_empty() {
            "-".to_string()
        } else {
            s.recent_latencies
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        let _ = writeln!(
            body,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            name,
            s.invocations,
            s.errors,
            s.denied,
            s.unknown_method,
            s.last_invoked_at,
            s.last_error_at
                .map(|x| x.to_string())
                .unwrap_or_else(|| "-".into()),
            s.latency_samples,
            s.last_elapsed_ms,
            s.max_elapsed_ms,
            mean,
            samples_csv,
        );
    }
    let _ = writeln!(body, "count={}", snap.len());
    body
}

/// Register node-type-specific capabilities based on `[controller] node_type`.
///
/// Advertise the four router.* capabilities in the manifest.
/// Called from `run()` only when `[controller] role = "router"`.
fn register_router_descriptors(manifest: &ManifestProvider) {
    use relix_core::capability::{CapabilityDescriptor, RiskLevel};
    manifest.add_capability(
        CapabilityDescriptor::unary("router.heartbeat")
            .with_description(
                "Controller-only: register or refresh this peer's liveness + capability list.",
            )
            .with_categories(["router".into(), "health".into()])
            .with_risk(RiskLevel::Low),
    );
    manifest.add_capability(
        CapabilityDescriptor::unary("router.network_summary")
            .with_description(
                "Operator-facing mesh overview: known peers, active sessions, uptime.",
            )
            .with_categories(["router".into(), "observability".into()])
            .with_risk(RiskLevel::Safe),
    );
    manifest.add_capability(
        CapabilityDescriptor::unary("router.session_list")
            .with_description(
                "Operator-facing session browser. Supports status filter + pagination.",
            )
            .with_categories(["router".into(), "observability".into()])
            .with_risk(RiskLevel::Safe),
    );
    manifest.add_capability(
        CapabilityDescriptor::unary("router.log")
            .with_description(
                "Controller-only: push a structured log line to the router for aggregation.",
            )
            .with_categories(["router".into(), "observability".into()])
            .with_risk(RiskLevel::Low),
    );
}

/// Spawn the 60-second heartbeat sender background task.
///
/// Behaviour:
/// - Wait 1.5 seconds after startup, then fire the initial heartbeat.
/// - Then loop with a 60-second `tokio::time::interval`.
/// - Each tick: build a [`relix_core::router::HeartbeatRequest`],
///   CBOR-encode it, sign as an identity-bearing
///   [`crate::dispatch::build_request`] envelope, send via the
///   transport client to `router_peer`.
/// - Identity bundle is loaded once at task start from
///   `<key_path>.bundle`. If the file is missing the heartbeat
///   sender logs a single WARN and exits cleanly — the
///   controller is still alive on the mesh and operator can
///   issue a bundle later.
/// - Each send: success at DEBUG, failure at WARN. The router
///   being down is non-fatal.
fn spawn_heartbeat_sender(
    client: rpc::Client,
    router_peer: rpc::PeerId,
    node_name: String,
    local_peer_id: String,
    manifest: ManifestProvider,
    key_path: std::path::PathBuf,
) {
    let bundle_path = key_path.with_extension("bundle");
    tokio::spawn(async move {
        // Load + decode the identity bundle once.
        let bundle_bytes = match std::fs::read(&bundle_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    bundle_path = %bundle_path.display(),
                    error = %e,
                    "heartbeat sender: identity bundle missing; heartbeats disabled (run `relix-cli identity issue` to create one)"
                );
                return;
            }
        };
        let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    bundle_path = %bundle_path.display(),
                    error = %e,
                    "heartbeat sender: identity bundle decode failed; heartbeats disabled"
                );
                return;
            }
        };
        // Extract groups from the bundle payload for the heartbeat body.
        let groups: Vec<String> = match relix_core::codec::decode::<
            relix_core::identity::IdentityBundle,
        >(&bundle.payload)
        {
            Ok(id) => id.groups,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "heartbeat sender: identity payload decode failed; sending with empty groups"
                );
                Vec::new()
            }
        };
        // Initial 1.5s warmup.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        // First tick fires immediately — that's our post-warmup heartbeat.
        loop {
            tick.tick().await;
            let req = relix_core::router::HeartbeatRequest {
                peer_id: local_peer_id.clone(),
                name: node_name.clone(),
                capabilities: manifest
                    .snapshot()
                    .capabilities
                    .iter()
                    .map(|c| c.method_name.clone())
                    .collect(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                groups: groups.clone(),
            };
            let args = match relix_core::codec::encode(&req) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "heartbeat encode failed; skipping tick");
                    continue;
                }
            };
            let envelope =
                crate::dispatch::build_request("router.heartbeat", args, bundle.clone(), 30);
            match client.call(router_peer, envelope).await {
                Ok(_) => tracing::debug!(router = %router_peer, "heartbeat sent"),
                Err(e) => tracing::warn!(
                    router = %router_peer,
                    error = %e,
                    "heartbeat send failed (router down? non-fatal)"
                ),
            }
        }
    });
}

/// Build the AI controller's outbound MeshClient and populate
/// the memory `OnceCell` so `ai.chat` starts injecting frozen-
/// snapshot memory. Silent failure — the AI node keeps serving
/// chat unaffected if the memory peer is unreachable or the
/// identity bundle isn't on disk yet.
async fn populate_ai_memory_cell(
    cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::ai::MemoryFetcher>>>,
    cfg: crate::nodes::ai::AiMemoryPeerConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    // Load the controller's identity bundle. The heartbeat
    // sender uses the same pattern (key_path + ".bundle"). Bail
    // silently if missing.
    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "ai memory dispatcher: identity bundle missing; memory injection disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ai memory dispatcher: identity bundle decode failed; memory injection disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "ai memory dispatcher: client key not 32 bytes; memory injection disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "ai memory dispatcher: client key missing; memory injection disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.alias.clone(),
        PeerEntry {
            addr: cfg.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                alias = %cfg.alias,
                addr = %cfg.addr,
                "ai memory dispatcher: discover_and_pin returned None; memory injection disabled"
            );
            return;
        }
    };
    let dispatcher: Arc<dyn crate::nodes::ai::MemoryFetcher> =
        Arc::new(crate::nodes::ai::MemoryDispatcher::new(
            mesh,
            cfg.alias.clone(),
            bundle,
            cfg.deadline_secs,
            cfg.max_history_turns,
            cfg.rag_enabled,
            cfg.rag_top_k,
            cfg.rag_min_score,
        ));
    if cell.set(dispatcher).is_err() {
        tracing::warn!("ai memory dispatcher: cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            alias = %cfg.alias,
            addr = %cfg.addr,
            "ai node: memory dispatcher online; frozen-snapshot injection active"
        );
    }
}

/// Build the memory controller's outbound MeshClient pointed
/// at the AI peer and populate the curator's `OnceCell`. Same
/// shape as `populate_ai_memory_cell`. Silent failure — the
/// memory node keeps serving reads/writes unaffected if the AI
/// peer is unreachable or the bundle is missing; the curator
/// scheduler will keep ticking and just skip every agent
/// (`memory curator: AI dispatcher not yet ready`).
async fn run_message_expire_loop(
    message_store: Arc<crate::nodes::coordinator::messaging::MessageStore>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match message_store.expire_due(now) {
            Ok(0) => {}
            Ok(n) => tracing::info!(expired = n, "msg expire: flipped past-ttl rows to expired"),
            Err(e) => tracing::warn!(error = %e, "msg expire: sweep failed"),
        }
    }
}

/// Register every `msg.*` capability + write a `msg.sent`
/// chronicle event on the coordinator's bookkeeping task after
/// each successful send. The chronicle write is best-effort —
/// failure does not propagate to the caller.
fn register_messaging_capabilities(
    bridge: &mut crate::dispatch::DispatchBridge,
    message_store: Arc<crate::nodes::coordinator::messaging::MessageStore>,
    task_store: Arc<crate::nodes::coordinator::TaskStore>,
) {
    use crate::dispatch::{FnHandler, HandlerOutcome, InvocationCtx};
    use crate::nodes::coordinator::messaging::handlers;

    // Ensure a single "msg-bookkeeping" task exists so the
    // msg.sent chronicle has somewhere to land. The lookup
    // pages through existing task rows once at register time;
    // creation is idempotent — re-running on the same db
    // reuses the existing row.
    let bookkeeping_task_id = ensure_msg_bookkeeping_task(&task_store);

    {
        let s = message_store.clone();
        let ts = task_store.clone();
        let book = bookkeeping_task_id.clone();
        bridge.register(
            "msg.send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let ts = ts.clone();
                let book = book.clone();
                async move {
                    let outcome = handlers::handle_send(&s, &ctx);
                    // Best-effort `msg.sent` chronicle event on
                    // the bookkeeping task — capture from / to /
                    // thread without the body so audit stays
                    // body-redacted.
                    if let (HandlerOutcome::Ok(body), Some(task_id)) = (&outcome, book.as_deref())
                        && let Ok(msg_id) = std::str::from_utf8(body)
                    {
                        let msg_id = msg_id.trim();
                        if let Ok(Some(rec)) = s.get(msg_id) {
                            let payload = format!(
                                "from={}|to={}|thread={}",
                                short_subject(&rec.from_subject_id),
                                short_subject(&rec.to_subject_id),
                                rec.thread_id
                            );
                            let _ = ts.append_event(task_id, "msg.sent", &payload);
                        }
                    }
                    outcome
                }
            })),
        );
    }
    {
        let s = message_store.clone();
        bridge.register(
            "msg.inbox",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_inbox(&s, &ctx) }
            })),
        );
    }
    {
        let s = message_store.clone();
        bridge.register(
            "msg.read",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_read(&s, &ctx) }
            })),
        );
    }
    {
        let s = message_store.clone();
        bridge.register(
            "msg.thread",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_thread(&s, &ctx) }
            })),
        );
    }
    {
        let s = message_store.clone();
        bridge.register(
            "msg.delete",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_delete(&s, &ctx) }
            })),
        );
    }
}

fn short_subject(s: &str) -> String {
    let cleaned: String = s.replace('|', "_");
    cleaned.chars().take(16).collect()
}

/// Ensure the coordinator hosts a single bookkeeping task
/// titled `msg-bookkeeping-system` so the `msg.sent`
/// chronicle event has somewhere to land. Returns the task_id
/// on success; logs + returns None on any storage hiccup
/// (the messaging capabilities still work; just the audit
/// event is skipped).
fn ensure_msg_bookkeeping_task(
    task_store: &Arc<crate::nodes::coordinator::TaskStore>,
) -> Option<String> {
    const TITLE: &str = "msg-bookkeeping-system";
    const FLOW: &str = "system:messaging";
    // Page through task summaries looking for the sentinel
    // title; reuse if present. (Same approach the memory
    // curator uses for its bookkeeping task.)
    let mut offset = 0usize;
    for _ in 0..5 {
        let rows = match task_store.list_paginated(200, offset, None) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "msg bookkeeping: task.list failed");
                return None;
            }
        };
        if rows.is_empty() {
            break;
        }
        for r in &rows {
            if r.title == TITLE {
                return Some(r.task_id.clone());
            }
        }
        offset += rows.len();
    }
    match task_store.create(
        TITLE,
        FLOW,
        "{}",
        "system",
        crate::nodes::coordinator::RetryPolicy::None,
        0,
        None,
        Some("scheduler"),
    ) {
        Ok(id) => {
            tracing::info!(task_id = %id, "msg bookkeeping: created system task");
            Some(id)
        }
        Err(e) => {
            tracing::warn!(error = %e, "msg bookkeeping: create failed");
            None
        }
    }
}

/// DEFERRED B: post-startup pass that fails any task linked to a
/// `legacy_token_expired` approval row.
///
/// Runs once AFTER `AgentStore::open` (which flipped the
/// approval rows themselves) and AFTER the TaskStore is alive.
/// For each `legacy_token_expired` row with a `task_id`:
///
/// 1. Read the task's current status via `TaskStore::get`.
/// 2. Skip when the task is already in a terminal state
///    (`completed | failed | cancelled`) — operators that have
///    already handled the parked task must not see their
///    decision overwritten.
/// 3. Otherwise transition to `failed` with
///    `error_cause = "legacy_approval_token_expired"` via the
///    canonical `TaskStore::update` path so the state machine
///    runs every guard + the chronicle event lands.
///
/// Idempotent: re-running matches the same approval set, but
/// the per-task terminal check guarantees only the first run
/// transitions any given task.
/// Stable name for the legacy-token orphaned-task fail pass in
/// the `startup_tasks` ledger. Pulled out so tests / docs / the
/// integration harness reference the same string.
#[doc(hidden)]
pub const LEGACY_TOKEN_TASK_FAIL_PASS_NAME: &str = "legacy_token_orphaned_task_fail";

/// NOT-DONE 2: progress checkpoint interval. Every N rows the
/// background pass records the current cursor + processed
/// count + logs at INFO so operators see the high-water mark.
pub(crate) const LEGACY_TOKEN_PASS_PROGRESS_INTERVAL: usize = 100;

/// NOT-DONE 2: results of one invocation of the
/// legacy-token-orphaned-task fail pass. Returned by
/// [`run_legacy_token_orphaned_task_fail_pass`] so tests can
/// assert exact counters; the background runner spawns it +
/// drops the result.
#[doc(hidden)]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LegacyTokenPassReport {
    pub transitioned: usize,
    pub already_terminal: usize,
    pub not_found: usize,
    pub errored: usize,
    pub progress_checkpoints: usize,
    pub considered: usize,
    pub started_at_resume_cursor: String,
    pub final_cursor: String,
    pub completed: bool,
}

/// NOT-DONE 2: async, resumable, non-blocking version of the
/// previous synchronous legacy-token-orphaned-task fail pass.
/// Spawned via `tokio::spawn` after the bridge wiring
/// completes so the controller never blocks on it. Skips when
/// `startup_tasks` records a prior completion; resumes from
/// `last_processed_id` after interruption; per-row errors are
/// logged and skipped, never aborting the whole pass; progress
/// checkpoints land in the SQLite ledger every
/// [`LEGACY_TOKEN_PASS_PROGRESS_INTERVAL`] rows.
#[doc(hidden)]
pub async fn run_legacy_token_orphaned_task_fail_pass(
    agent_store: std::sync::Arc<crate::nodes::coordinator::agent::AgentStore>,
    task_store: std::sync::Arc<crate::nodes::coordinator::TaskStore>,
    clock: std::sync::Arc<dyn relix_core::clock::Clock>,
) -> LegacyTokenPassReport {
    let mut report = LegacyTokenPassReport::default();
    match agent_store.startup_task_is_complete(LEGACY_TOKEN_TASK_FAIL_PASS_NAME) {
        Ok(true) => {
            tracing::info!(
                pass = LEGACY_TOKEN_TASK_FAIL_PASS_NAME,
                "approval: legacy-token orphaned-task fail pass already complete; skipping"
            );
            report.completed = true;
            return report;
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(error = %e, "legacy-token pass: startup_task_is_complete failed");
            return report;
        }
    }
    let resume_cursor = match agent_store.startup_task_get(LEGACY_TOKEN_TASK_FAIL_PASS_NAME) {
        Ok(Some(row)) => row.last_processed_id.unwrap_or_default(),
        Ok(None) => String::new(),
        Err(e) => {
            tracing::warn!(error = %e, "legacy-token pass: startup_task_get failed");
            return report;
        }
    };
    report.started_at_resume_cursor = resume_cursor.clone();
    if let Err(e) = agent_store.startup_task_begin(LEGACY_TOKEN_TASK_FAIL_PASS_NAME, clock.now_ms())
    {
        tracing::warn!(error = %e, "legacy-token pass: startup_task_begin failed");
        return report;
    }
    let mut rows_processed: i64 =
        match agent_store.startup_task_get(LEGACY_TOKEN_TASK_FAIL_PASS_NAME) {
            Ok(Some(r)) => r.rows_processed,
            _ => 0,
        };
    let pairs = match agent_store.list_legacy_token_expired_after(&resume_cursor) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "legacy-token pass: list_legacy_token_expired_after failed");
            return report;
        }
    };
    tracing::info!(
        pass = LEGACY_TOKEN_TASK_FAIL_PASS_NAME,
        candidates = pairs.len(),
        resume_cursor = %resume_cursor,
        "approval: legacy-token orphaned-task fail pass starting"
    );
    report.considered = pairs.len();
    for (i, (approval_id, task_id)) in pairs.iter().enumerate() {
        let view = match task_store.get(task_id) {
            Ok(Some(v)) => Some(v),
            Ok(None) => {
                report.not_found += 1;
                None
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    approval_id = %approval_id,
                    error = %e,
                    "legacy-token pass: task lookup failed (skipping)"
                );
                report.errored += 1;
                None
            }
        };
        if let Some(view) = view {
            let is_terminal = matches!(view.status.as_str(), "completed" | "failed" | "cancelled");
            if is_terminal {
                report.already_terminal += 1;
            } else {
                match task_store.update(
                    task_id,
                    Some("failed"),
                    None,
                    None,
                    None,
                    None,
                    Some("legacy_approval_token_expired"),
                    Some("legacy_approval_token_expired"),
                ) {
                    Ok(()) => {
                        let _ = task_store.append_event(
                            task_id,
                            "task.failed",
                            "legacy_approval_token_expired",
                        );
                        report.transitioned += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task_id,
                            approval_id = %approval_id,
                            error = %e,
                            "legacy-token pass: task transition failed (skipping)"
                        );
                        report.errored += 1;
                    }
                }
            }
        }
        rows_processed += 1;
        report.final_cursor = approval_id.clone();
        tokio::task::yield_now().await;
        if (i + 1) % LEGACY_TOKEN_PASS_PROGRESS_INTERVAL == 0 {
            tracing::info!(
                pass = LEGACY_TOKEN_TASK_FAIL_PASS_NAME,
                rows_processed,
                last_processed_id = %approval_id,
                transitioned = report.transitioned,
                already_terminal = report.already_terminal,
                not_found = report.not_found,
                errored = report.errored,
                "approval: legacy-token pass progress"
            );
            report.progress_checkpoints += 1;
            if let Err(e) = agent_store.startup_task_record_progress(
                LEGACY_TOKEN_TASK_FAIL_PASS_NAME,
                approval_id,
                rows_processed,
            ) {
                tracing::warn!(error = %e, "legacy-token pass: record_progress failed");
            }
        }
    }
    if !report.final_cursor.is_empty()
        && let Err(e) = agent_store.startup_task_record_progress(
            LEGACY_TOKEN_TASK_FAIL_PASS_NAME,
            &report.final_cursor,
            rows_processed,
        )
    {
        tracing::warn!(error = %e, "legacy-token pass: final record_progress failed");
    }
    match agent_store.startup_task_complete(
        LEGACY_TOKEN_TASK_FAIL_PASS_NAME,
        clock.now_ms(),
        rows_processed,
    ) {
        Ok(()) => {
            report.completed = true;
            tracing::info!(
                pass = LEGACY_TOKEN_TASK_FAIL_PASS_NAME,
                transitioned = report.transitioned,
                already_terminal = report.already_terminal,
                not_found = report.not_found,
                errored = report.errored,
                rows_processed,
                "approval: legacy-token orphaned-task fail pass complete"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "legacy-token pass: startup_task_complete failed");
        }
    }
    report
}

/// Legacy synchronous wrapper kept ONLY for the existing
/// test-mode call sites. Production wiring now spawns
/// [`run_legacy_token_orphaned_task_fail_pass`] via
/// `tokio::spawn` after controller boot.
#[cfg(test)]
fn fail_tasks_orphaned_by_legacy_token_migration(
    agent_store: &std::sync::Arc<crate::nodes::coordinator::agent::AgentStore>,
    task_store: &std::sync::Arc<crate::nodes::coordinator::TaskStore>,
) {
    let task_ids = match agent_store.list_legacy_token_expired_task_ids() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "legacy-token migration: task-fail pass — list failed");
            return;
        }
    };
    if task_ids.is_empty() {
        return;
    }
    let mut transitioned = 0usize;
    let mut already_terminal = 0usize;
    let mut not_found = 0usize;
    for tid in &task_ids {
        let view = match task_store.get(tid) {
            Ok(Some(v)) => v,
            Ok(None) => {
                not_found += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(task_id = %tid, error = %e, "legacy-token migration: task lookup failed");
                continue;
            }
        };
        // Mirror of `TaskStore`'s terminal-status check (kept
        // in lock-step with the dispatcher; see
        // `close_orphan_attempts_closes_attempts_whose_task_is_terminal`).
        let is_terminal = matches!(view.status.as_str(), "completed" | "failed" | "cancelled");
        if is_terminal {
            already_terminal += 1;
            continue;
        }
        match task_store.update(
            tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            Some("legacy_approval_token_expired"),
            Some("legacy_approval_token_expired"),
        ) {
            Ok(()) => {
                let _ =
                    task_store.append_event(tid, "task.failed", "legacy_approval_token_expired");
                transitioned += 1;
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %tid,
                    error = %e,
                    "legacy-token migration: task transition to failed failed"
                );
            }
        }
    }
    tracing::warn!(
        transitioned,
        already_terminal,
        not_found,
        total = task_ids.len(),
        "approval: transitioned {transitioned} tasks to failed state due to legacy approval token migration ({already_terminal} already terminal, {not_found} not found)"
    );
}

#[cfg(test)]
mod legacy_token_task_fail_tests {
    use super::*;
    use crate::nodes::coordinator::TaskStore;
    use crate::nodes::coordinator::agent::AgentStore;

    fn stores() -> (Arc<AgentStore>, Arc<TaskStore>) {
        (
            Arc::new(AgentStore::in_memory().unwrap()),
            Arc::new(TaskStore::in_memory().unwrap()),
        )
    }

    fn create_task(ts: &TaskStore, title: &str) -> String {
        ts.create(
            title,
            "flow",
            "{}",
            "owner",
            crate::nodes::coordinator::RetryPolicy::None,
            0,
            None,
            None,
        )
        .expect("create task")
    }

    /// Seed a legacy `pending`-with-opaque-token approval row
    /// stamped with `task_id`, then run the boot-time
    /// migration so the row is now `legacy_token_expired`.
    fn seed_legacy_with_task(
        agent: &AgentStore,
        approval_id: &str,
        task_id: &str,
    ) -> Result<(), String> {
        agent
            .seed_legacy_token_row_for_test(approval_id, "pending", "deadbeef")
            .map_err(|e| e.to_string())?;
        agent
            .force_set_task_id_for_test(approval_id, task_id)
            .map_err(|e| e.to_string())?;
        let _ = agent
            .run_legacy_token_migration_for_test()
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    #[test]
    fn fail_pass_transitions_linked_task_to_failed() {
        let (agent, tasks) = stores();
        let task_id = create_task(&tasks, "fail-1 title");
        seed_legacy_with_task(&agent, "leg-1", &task_id).unwrap();
        fail_tasks_orphaned_by_legacy_token_migration(&agent, &tasks);
        let view = tasks.get(&task_id).unwrap().unwrap();
        assert_eq!(view.status, "failed");
        assert_eq!(
            view.error_cause.as_deref(),
            Some("legacy_approval_token_expired")
        );
    }

    #[test]
    fn fail_pass_skips_task_already_in_terminal_state() {
        let (agent, tasks) = stores();
        let task_id = create_task(&tasks, "done title");
        // Mark task `completed` BEFORE the migration pass.
        tasks
            .update(
                &task_id,
                Some("completed"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        seed_legacy_with_task(&agent, "leg-2", &task_id).unwrap();
        fail_tasks_orphaned_by_legacy_token_migration(&agent, &tasks);
        let view = tasks.get(&task_id).unwrap().unwrap();
        // Untouched: terminal state stays terminal.
        assert_eq!(view.status, "completed");
        assert!(view.error_cause.is_none());
    }

    #[test]
    fn fail_pass_is_idempotent_under_repeat_run() {
        let (agent, tasks) = stores();
        let task_id = create_task(&tasks, "idem title");
        seed_legacy_with_task(&agent, "leg-3", &task_id).unwrap();
        // First run: transition.
        fail_tasks_orphaned_by_legacy_token_migration(&agent, &tasks);
        let first = tasks.get(&task_id).unwrap().unwrap();
        let first_updated_at = first.updated_at;
        let first_error = first.error_cause.clone();
        assert_eq!(first.status, "failed");
        // Second run: task is now `failed` (terminal); the
        // per-task guard short-circuits + nothing else changes.
        fail_tasks_orphaned_by_legacy_token_migration(&agent, &tasks);
        let second = tasks.get(&task_id).unwrap().unwrap();
        assert_eq!(second.status, "failed");
        assert_eq!(second.error_cause, first_error);
        assert_eq!(
            second.updated_at, first_updated_at,
            "second run must NOT bump updated_at"
        );
    }

    #[test]
    fn fail_pass_skips_approvals_without_task_id() {
        // An approval with NULL task_id never blocked a task,
        // so the post-migration pass should not even consider
        // it. We assert via the agent store's list helper
        // returning nothing.
        let (agent, _tasks) = stores();
        agent
            .seed_legacy_token_row_for_test("leg-no-task", "pending", "abc")
            .unwrap();
        agent.run_legacy_token_migration_for_test().unwrap();
        let task_ids = agent.list_legacy_token_expired_task_ids().unwrap();
        assert!(
            task_ids.is_empty(),
            "row with NULL task_id must not be listed"
        );
    }

    // ── NOT-DONE 2: background-task pass + resume cursor ──

    fn fake_clock() -> Arc<relix_core::clock::FakeClock> {
        Arc::new(relix_core::clock::FakeClock::new(1_700_000_000_000))
    }

    fn dyn_clock(c: &Arc<relix_core::clock::FakeClock>) -> Arc<dyn relix_core::clock::Clock> {
        c.clone()
    }

    /// Seed N legacy approvals each linked to a fresh task, in
    /// id-ascending order suitable for cursor walks. Returns
    /// the task ids in the same order so the test can audit
    /// individual transitions.
    fn seed_n_legacy_with_tasks(
        agent: &AgentStore,
        tasks: &TaskStore,
        n: usize,
        approval_prefix: &str,
    ) -> Vec<String> {
        let mut task_ids = Vec::with_capacity(n);
        for i in 0..n {
            // Zero-pad so lexicographic ASC ordering matches
            // numeric. 4 digits handles up to 9_999 rows.
            let approval_id = format!("{approval_prefix}-{i:04}");
            let task_id = create_task(tasks, &format!("task-{approval_id}"));
            // `approval_token` column carries a UNIQUE
            // constraint — synthesise a per-row token so the
            // seed loop does not collide on the second
            // insert.
            let token = format!("legacy-{approval_prefix}-{i:08}");
            agent
                .seed_legacy_token_row_for_test(&approval_id, "pending", &token)
                .unwrap();
            agent
                .force_set_task_id_for_test(&approval_id, &task_id)
                .unwrap();
            task_ids.push(task_id);
        }
        agent.run_legacy_token_migration_for_test().unwrap();
        task_ids
    }

    #[tokio::test]
    async fn background_pass_does_not_block_caller() {
        // Seed enough rows that the pass would visibly stall if
        // it ran synchronously, then spawn it and immediately
        // do "controller work" alongside. The pass must run
        // off the calling task's progress path.
        let (agent, tasks) = stores();
        let _ = seed_n_legacy_with_tasks(&agent, &tasks, 250, "bg");
        let clock = fake_clock();
        let agent_for_spawn = agent.clone();
        let tasks_for_spawn = tasks.clone();
        let clock_for_spawn = dyn_clock(&clock);
        let handle = tokio::spawn(async move {
            run_legacy_token_orphaned_task_fail_pass(
                agent_for_spawn,
                tasks_for_spawn,
                clock_for_spawn,
            )
            .await
        });
        // "Controller work": read the agent store while the
        // pass runs. This races the pass intentionally — both
        // must complete cleanly.
        for _ in 0..20 {
            let _ = agent.list_legacy_token_expired_task_ids().unwrap();
            tokio::task::yield_now().await;
        }
        let report = handle.await.expect("background task joins");
        assert!(report.completed);
        assert_eq!(report.transitioned, 250);
        assert_eq!(report.considered, 250);
        // 250 rows / 100-row checkpoint = 2 checkpoints
        // (rows 100 and 200; row 250 only writes the final
        // cursor outside the checkpoint loop).
        assert_eq!(report.progress_checkpoints, 2);
    }

    #[tokio::test]
    async fn interrupted_pass_resumes_from_last_processed_id_on_next_run() {
        // Manually seed `startup_tasks` to mid-pass state —
        // simulates a process killed after `cursor = N-2`.
        let (agent, tasks) = stores();
        let task_ids = seed_n_legacy_with_tasks(&agent, &tasks, 5, "resume");
        agent
            .startup_task_begin(LEGACY_TOKEN_TASK_FAIL_PASS_NAME, 1_700_000_000_000)
            .unwrap();
        // Simulate that rows 0 + 1 + 2 were processed before
        // interruption: cursor = "resume-0002", rows_processed = 3.
        // The first three tasks are NOT actually transitioned
        // in this fixture — the resume contract is "skip
        // anything ≤ cursor"; whether the cursor's predecessors
        // got their state machine update is a property of the
        // pre-interruption run, not what this test is verifying.
        agent
            .startup_task_record_progress(LEGACY_TOKEN_TASK_FAIL_PASS_NAME, "resume-0002", 3)
            .unwrap();
        let clock = fake_clock();
        let report = run_legacy_token_orphaned_task_fail_pass(
            agent.clone(),
            tasks.clone(),
            dyn_clock(&clock),
        )
        .await;
        assert!(report.completed);
        assert_eq!(report.started_at_resume_cursor, "resume-0002");
        // Two rows left to process after the cursor.
        assert_eq!(report.considered, 2);
        assert_eq!(report.transitioned, 2);
        assert_eq!(report.final_cursor, "resume-0004");
        // Tasks at indices 0/1/2 were NOT transitioned (the
        // resume contract says skip pre-cursor); 3/4 were.
        assert_eq!(tasks.get(&task_ids[0]).unwrap().unwrap().status, "pending");
        assert_eq!(tasks.get(&task_ids[3]).unwrap().unwrap().status, "failed");
        assert_eq!(tasks.get(&task_ids[4]).unwrap().unwrap().status, "failed");
    }

    #[tokio::test]
    async fn completed_pass_is_not_re_run_on_next_boot() {
        let (agent, tasks) = stores();
        let task_ids = seed_n_legacy_with_tasks(&agent, &tasks, 3, "comp");
        let clock = fake_clock();
        // First run — completes.
        let first = run_legacy_token_orphaned_task_fail_pass(
            agent.clone(),
            tasks.clone(),
            dyn_clock(&clock),
        )
        .await;
        assert!(first.completed);
        assert_eq!(first.transitioned, 3);
        // Second run — short-circuits via startup_task_is_complete.
        let second = run_legacy_token_orphaned_task_fail_pass(
            agent.clone(),
            tasks.clone(),
            dyn_clock(&clock),
        )
        .await;
        assert!(second.completed);
        assert_eq!(
            second.considered, 0,
            "second run must not consider any rows: {second:?}"
        );
        assert_eq!(second.transitioned, 0);
        for tid in &task_ids {
            // First-run side effects are preserved.
            assert_eq!(tasks.get(tid).unwrap().unwrap().status, "failed");
        }
    }

    #[tokio::test]
    async fn per_row_error_does_not_abort_the_whole_pass() {
        // Seed three rows; for the middle one, point the
        // approval row at a task_id the TaskStore does NOT
        // have (so `task_store.get` returns Ok(None) →
        // `not_found += 1`). The remaining rows must still
        // transition.
        let (agent, tasks) = stores();
        let t0 = create_task(&tasks, "row-0");
        let t2 = create_task(&tasks, "row-2");
        // Row 0: valid task.
        agent
            .seed_legacy_token_row_for_test("err-0000", "pending", "abc-0000")
            .unwrap();
        agent.force_set_task_id_for_test("err-0000", &t0).unwrap();
        // Row 1: task_id that does not exist in the TaskStore.
        agent
            .seed_legacy_token_row_for_test("err-0001", "pending", "abc-0001")
            .unwrap();
        agent
            .force_set_task_id_for_test("err-0001", "nonexistent_task_id_xyz")
            .unwrap();
        // Row 2: valid task.
        agent
            .seed_legacy_token_row_for_test("err-0002", "pending", "abc-0002")
            .unwrap();
        agent.force_set_task_id_for_test("err-0002", &t2).unwrap();
        agent.run_legacy_token_migration_for_test().unwrap();
        let clock = fake_clock();
        let report = run_legacy_token_orphaned_task_fail_pass(
            agent.clone(),
            tasks.clone(),
            dyn_clock(&clock),
        )
        .await;
        assert!(report.completed, "pass must complete despite errors");
        assert_eq!(report.considered, 3);
        assert_eq!(report.transitioned, 2, "rows 0 + 2 transition");
        assert_eq!(report.not_found, 1, "row 1 was missing in TaskStore");
        assert_eq!(report.errored, 0, "not_found is not counted as errored");
        assert_eq!(tasks.get(&t0).unwrap().unwrap().status, "failed");
        assert_eq!(tasks.get(&t2).unwrap().unwrap().status, "failed");
    }

    #[tokio::test]
    async fn progress_is_checkpointed_every_n_rows() {
        // Seed enough rows for two full checkpoint intervals
        // (200) plus a remainder (50). Verify the
        // `progress_checkpoints` counter on the report AND the
        // SQLite ledger advanced past `rows_processed = 200`.
        let n = 2 * LEGACY_TOKEN_PASS_PROGRESS_INTERVAL + 50;
        let (agent, tasks) = stores();
        let _ = seed_n_legacy_with_tasks(&agent, &tasks, n, "prog");
        let clock = fake_clock();
        let report = run_legacy_token_orphaned_task_fail_pass(
            agent.clone(),
            tasks.clone(),
            dyn_clock(&clock),
        )
        .await;
        assert!(report.completed);
        assert_eq!(report.considered, n);
        // Two scheduled checkpoints (rows 100, 200). The final
        // 50 rows trigger only the end-of-run cursor write.
        assert_eq!(report.progress_checkpoints, 2);
        let row = agent
            .startup_task_get(LEGACY_TOKEN_TASK_FAIL_PASS_NAME)
            .unwrap()
            .expect("ledger row");
        assert_eq!(row.rows_processed, n as i64);
        assert!(row.completed_at_ms.is_some());
    }
}

async fn run_approval_expire_loop(
    agent_store: Arc<crate::nodes::coordinator::agent::AgentStore>,
    task_store: Arc<crate::nodes::coordinator::TaskStore>,
    clock: Arc<dyn relix_core::clock::Clock>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        // NOT-DONE 1: source the expiry-window edge from the
        // injected clock so tests can drive the legacy-token
        // expiry sweep deterministically via `FakeClock` +
        // `tokio::time::advance` on the interval.
        let now = clock.now_ms() / 1_000;
        let expired = match agent_store.list_expired_pending(now) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "approval expire: list_expired_pending failed");
                continue;
            }
        };
        for (approval_id, task_id) in expired {
            if let Err(e) = agent_store.mark_expired(&approval_id) {
                tracing::warn!(error = %e, "approval expire: mark_expired failed");
                continue;
            }
            if let Some(tid) = task_id.as_deref() {
                let _ = task_store.append_event(
                    tid,
                    "task.approval_expired",
                    &format!("approval_id={approval_id}"),
                );
                let _ = task_store.update(
                    tid,
                    Some("failed"),
                    Some("approval expired"),
                    None,
                    None,
                    None,
                    None,
                    Some("approval_timeout"),
                );
            }
            tracing::info!(approval_id = %approval_id, "approval expired");
        }
    }
}

/// Register every `agent.*` / `coord.approval.*` /
/// `agent.standing_approval.*` capability on the coordinator's
/// dispatch bridge. The CRUD handlers run synchronously; the
/// approval-decide handler captures closures that flip the
/// waiting task back to running / failed and append the
/// corresponding chronicle event.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn register_agent_capabilities(
    bridge: &mut crate::dispatch::DispatchBridge,
    agent_store: Arc<crate::nodes::coordinator::agent::AgentStore>,
    task_store: Arc<crate::nodes::coordinator::TaskStore>,
    spine_store: Option<Arc<crate::nodes::coordinator::spine::SpineStore>>,
    token_ttl_secs: u64,
    clock: Arc<dyn relix_core::clock::Clock>,
    descriptor_cache: crate::manifest::DescriptorCache,
    // Authoritative live-spend ledger for the Action Center budget alerts — the
    // SAME `MetricsQuery::cost_since` source the dispatch gate enforces. `None`
    // when metrics are disabled, in which case the feed shows allowance-backed
    // budget signals only (never a fabricated spend figure).
    metrics_query: Option<crate::metrics::MetricsQuery>,
) {
    use crate::dispatch::{FnHandler, InvocationCtx};
    use crate::nodes::coordinator::agent::handlers;
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.create",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_create(&s, &ctx) }
            })),
        );
    }
    // First-run owner/Founder bootstrap (company-model: the Founder is
    // the apex Operative). `company.status` is a read; `company.
    // bootstrap_founder` is the owner-gated, idempotent first-run action
    // that stands up the single Founder; `agent.operatives` is the
    // tenant-scoped Crew roster (excludes the infra operator-console).
    {
        // `company.status` carries a read-only, tenant-scoped operations summary
        // (work in flight / blocked / review / approvals / mandates) when the
        // spine + task stores are available — which they always are in the live
        // bridge. It degrades to the agent-only first-run read if the spine is
        // absent. The summary derives ONLY from existing tenant-scoped reads and
        // mutates nothing (company-model §5.4 / §8.2; dashboard-design §5).
        let s = agent_store.clone();
        let ts = task_store.clone();
        let spine = spine_store.clone();
        bridge.register(
            "company.status",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let ts = ts.clone();
                let spine = spine.clone();
                async move {
                    match spine {
                        Some(spine) => {
                            handlers::handle_company_status_with_ops(&s, &spine, &ts, &ctx)
                        }
                        None => handlers::handle_company_status(&s, &ctx),
                    }
                }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "company.bootstrap_founder",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_bootstrap_founder(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "company.starter_crew",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_starter_crew(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.operatives",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_operatives(&s, &ctx) }
            })),
        );
    }
    // PHASE 4 (hire flow): the gated creation path (pending → approve).
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.request_hire",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_request_hire(&s, &ctx) }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        bridge.register(
            "agent.request_hire_for_mandate",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                async move { handlers::handle_request_hire_for_mandate(&s, &spine, &ctx) }
            })),
        );
    }
    // PRIME: governed team-build foundation (strategy + spawn Key gated).
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        bridge.register(
            "mandate.team_plan",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                async move { handlers::handle_team_plan(&s, &spine, &ctx) }
            })),
        );
    }
    // PRIME: read the latest persisted Team Plan for a Mandate.
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "mandate.team_plan.latest",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_team_plan_latest(&spine, &ctx) }
            })),
        );
    }
    // PRIME: Mandate strategy gate — propose / approve / reject / status.
    // These expose the existing strategy store so the dashboard can drive a
    // Mandate blocked → planned → ready WITHOUT bypassing governance.
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "mandate.strategy.status",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_strategy_status(&spine, &ctx) }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "mandate.strategy.propose",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_strategy_propose(&spine, &ctx) }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "mandate.strategy.approve",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_strategy_approve(&spine, &ctx) }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "mandate.strategy.reject",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_strategy_reject(&spine, &ctx) }
            })),
        );
    }
    // PRIME: live team readiness (plan + current hire/Clearance states).
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        bridge.register(
            "mandate.team_readiness",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                async move { handlers::handle_team_readiness(&s, &spine, &ctx) }
            })),
        );
    }
    // PRIME: Mandate-to-Brief orchestration (strategy + ready-team gated).
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        let ts = task_store.clone();
        bridge.register(
            "mandate.orchestrate",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                let ts = ts.clone();
                async move { handlers::handle_orchestrate(&ts, &s, &spine, &ctx) }
            })),
        );
    }
    // PRIME ASSISTANT: governed "describe what you want → plan" surface.
    // `prime.propose` is READ-ONLY (writes only the proposal record);
    // `prime.approve` is the ONLY path that creates the Mandate + Briefs +
    // pending hire requests. Both tenant-scoped.
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        bridge.register(
            "prime.propose",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                async move { handlers::handle_prime_propose(&s, &spine, &ctx) }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        let ts = task_store.clone();
        bridge.register(
            "prime.approve",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                let ts = ts.clone();
                async move { handlers::handle_prime_approve(&s, &spine, &ts, &ctx) }
            })),
        );
    }
    // PRIME ASSISTANT: read the proposal history / one proposal.
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "prime.proposals",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_prime_proposals(&spine, &ctx) }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "prime.proposal",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_prime_proposal_get(&spine, &ctx) }
            })),
        );
    }
    // PRIME ASSISTANT: the LIVE Shift-Room status of one work session
    // (proposal). READ-ONLY — joins the proposal row, the Brief board, and the
    // run ledger into a single command-center payload. Tenant-scoped.
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        let ts = task_store.clone();
        bridge.register(
            "prime.status",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                let ts = ts.clone();
                async move { handlers::handle_prime_status(&s, &spine, &ts, &ctx) }
            })),
        );
    }
    // PRIME GUIDED DRIVER v1 (company-model §5.4/§8.2 — the Action Center's
    // "next governed step" focused onto a single Prime work session; §12.5/§12.5B
    // — the Prime planner + prime.start). `prime.next_step` is READ-ONLY (classify
    // the one next step for a proposal/mandate over live state); `prime.advance`
    // executes AT MOST ONE safe, explicitly-requested governed step
    // (`create_team_plan` / `orchestrate_assign_ready`) by re-reading state and
    // refusing if it is stale — it NEVER auto-approves a strategy/hire/spawn/budget
    // gate and NEVER runs a real adapter. Both tenant-scoped.
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        let ts = task_store.clone();
        bridge.register(
            "prime.next_step",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                let ts = ts.clone();
                async move {
                    crate::nodes::coordinator::agent::prime_driver::handle_prime_next_step(
                        &s, &spine, &ts, &ctx,
                    )
                }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        let ts = task_store.clone();
        bridge.register(
            "prime.advance",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                let ts = ts.clone();
                async move {
                    crate::nodes::coordinator::agent::prime_driver::handle_prime_advance(
                        &s, &spine, &ts, &ctx,
                    )
                }
            })),
        );
    }
    // PRIME STANDING AUTHORITY (v1) — READ-ONLY. Reports, for the caller's Guild,
    // whether each of the three autonomous-Prime standing-authority categories
    // (proposal/hire/clearance approve) is currently active, plus the synthetic
    // authority id + categories operators grant via the existing
    // `agent.standing_approval.*` routes. Tenant-scoped; mutates nothing.
    {
        let s = agent_store.clone();
        bridge.register(
            "prime.standing_authority",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move {
                    crate::nodes::coordinator::agent::prime_driver::handle_prime_standing_authority(
                        &s, &ctx,
                    )
                }
            })),
        );
    }
    // PRIME RUNTIME AUTONOMY SWITCH (v1) — turn the autonomous Prime LOOP
    // ON/OFF for the caller's Guild at runtime (no restart), persisted in the
    // SpineStore. `prime.autonomy_state` is READ-ONLY (effective state + source
    // + env override + knobs); `prime.autonomy_set` is the role-gated mutation
    // (`{enabled:bool}`). NOT an approval bypass — ON only wakes the loop over
    // already-approved work; each governed approval still needs a live standing
    // grant. Both tenant-scoped; need the SpineStore.
    if let Some(spine) = spine_store.clone() {
        let s = spine.clone();
        bridge.register(
            "prime.autonomy_state",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move {
                    crate::nodes::coordinator::agent::prime_driver::handle_prime_autonomy_state(
                        &s, &ctx,
                    )
                }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        let s = spine.clone();
        bridge.register(
            "prime.autonomy_set",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move {
                    crate::nodes::coordinator::agent::prime_driver::handle_prime_autonomy_set(
                        &s, &ctx,
                    )
                }
            })),
        );
    }
    // ACTION CENTER (company-model §5.4 / §8.2): one READ-ONLY feed of the
    // operator's next actions, computed from existing live state (approvals,
    // hires, the Brief board, the run ledger, the strategy gate). Tenant-scoped;
    // mutates nothing. Needs the agent + spine + task stores.
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        let ts = task_store.clone();
        let mq = metrics_query.clone();
        bridge.register(
            "company.actions",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                let ts = ts.clone();
                let mq = mq.clone();
                async move {
                    // Live-spend seam: the SAME metrics ledger + canonical
                    // calendar-month window the dispatch gate enforces
                    // (`heartbeat::allowance_window`). `None` → allowance-backed
                    // budget signals only (no fabricated spend figure).
                    let spend = mq.map(handlers::MetricsSpendSource::current_month);
                    let spend_ref: Option<
                        &dyn crate::nodes::coordinator::agent::action_center::SpendSource,
                    > = spend.as_ref().map(|s| {
                        s as &dyn crate::nodes::coordinator::agent::action_center::SpendSource
                    });
                    handlers::handle_company_actions_with_spend(&s, &spine, &ts, spend_ref, &ctx)
                }
            })),
        );
    }
    // CANONICAL GUILD MONTH-TO-DATE SPEND (company-model §6.6 / §3.6;
    // dashboard-design §10): one numeric route the Costs page reads for the
    // Guild's actual month-to-date spend, computed from the SAME metrics ledger +
    // canonical calendar-month window the autonomous Guild hard-stop enforces
    // (`heartbeat::guild_spend_micros` over `heartbeat::allowance_window`).
    // Tenant-scoped; mutates nothing. `metrics == None` → honest null spend.
    if let Some(spine) = spine_store.clone() {
        let s = agent_store.clone();
        let mq = metrics_query.clone();
        bridge.register(
            "guild.spend",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let spine = spine.clone();
                let mq = mq.clone();
                async move {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    handlers::handle_guild_spend(&s, &spine, mq.as_ref(), now_ms, &ctx)
                }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "mandate.orchestration.latest",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_orchestration_latest(&spine, &ctx) }
            })),
        );
    }
    if let Some(spine) = spine_store.clone() {
        bridge.register(
            "mandate.orchestration.list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let spine = spine.clone();
                async move { handlers::handle_orchestration_list(&spine, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.approve_hire",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_approve_hire(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.reject_hire",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_reject_hire(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_get(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_list(&s, &ctx) }
            })),
        );
    }
    // PHASE 2 (org tree): Roster / Lattice reads over reports_to.
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.reports",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_reports(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.peers",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_peers(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.by_role",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_by_role(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.branch",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_branch(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.line",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_line(&s, &ctx) }
            })),
        );
    }
    // PHASE 2 (Keys panel): structured JSON profile read.
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.keys",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_keys(&s, &ctx) }
            })),
        );
    }
    // PHASE 2/3: delegated-authority check (Branch / subtree).
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.manages",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_manages(&s, &ctx) }
            })),
        );
    }
    // KEYS: queryable assign-Key verdict (actor → assignee). The
    // enforcement counterpart runs at `brief.set` (assignee).
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.assign_check",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_assign_check(&s, &ctx) }
            })),
        );
    }
    // PHASE 5 (companion): Roster-at-a-glance status counts.
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.roster_summary",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_roster_summary(&s, &ctx) }
            })),
        );
    }
    // PHASE 4 (Allowance oversight): committed allowance vs Guild budget.
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.allowance_committed",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_allowance_committed(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.update",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_update(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.delete",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_delete(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.effective_capabilities",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move {
                    handlers::handle_effective_capabilities(&s, &ctx, |_peer| {
                        // The coordinator doesn't carry a
                        // manifest cache of *other* peers, so the
                        // intersection runs against an empty
                        // capability set. The bridge proxy
                        // (PH-AGENT-BRIDGE) injects the cached
                        // manifest before forwarding.
                        Vec::new()
                    })
                }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "coord.approval.pending",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_approval_pending(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        let ts = task_store.clone();
        bridge.register(
            "brief.clearance_request",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let ts = ts.clone();
                async move { handlers::handle_brief_clearance_request(&s, &ts, &ctx) }
            })),
        );
    }
    {
        // DEFERRED 3: per-approval status read so a waiting
        // agent can distinguish `pending` from
        // `legacy_token_expired` (or any other terminal state).
        let s = agent_store.clone();
        bridge.register(
            "coord.approval.get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_approval_get(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        let ts_resume = task_store.clone();
        let ts_fail = task_store.clone();
        let resume: handlers::TaskResumeFn = Arc::new(move |task_id: &str| {
            // Resume the task: awaiting_input → running. Best-effort
            // — pause / freeze races leave the row in its current
            // state and the chronicle event still lands.
            let r = ts_resume.update(task_id, Some("running"), None, None, None, None, None, None);
            let _ = ts_resume.append_event(task_id, "task.approval_decided", "decision=approved");
            r.map_err(|e| e.to_string())
        });
        let fail: handlers::TaskResumeFn = Arc::new(move |task_id: &str| {
            let r = ts_fail.update(
                task_id,
                Some("failed"),
                Some("rejected via coord.approval.decide"),
                None,
                None,
                None,
                None,
                Some("approval_rejected"),
            );
            let _ = ts_fail.append_event(task_id, "task.approval_decided", "decision=rejected");
            r.map_err(|e| e.to_string())
        });
        // P1: capture the Ed25519 signer at register time so
        // the cap handler can mint approval tokens without
        // consulting env on every invocation. `None` when the
        // operator did not set `RELIX_APPROVAL_SIGNING_KEY` —
        // handler gracefully omits the token in that case.
        let signer: Option<crate::approval::ApprovalSigner> =
            crate::approval::signer_from_env().ok();
        // NOT-DONE 1: capture the dispatch clock so the
        // cap handler stamps `issued_at_ms` on each minted
        // token via the same time source the admission gate
        // verifies against.
        let clock_for_decide = clock.clone();
        bridge.register(
            "coord.approval.decide",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let resume = resume.clone();
                let fail = fail.clone();
                let signer = signer.clone();
                let clock = clock_for_decide.clone();
                async move {
                    handlers::handle_approval_decide(
                        &s,
                        &ctx,
                        &resume,
                        &fail,
                        signer.as_ref(),
                        token_ttl_secs,
                        clock.as_ref(),
                    )
                }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.standing_approval.create",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_standing_create(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.standing_approval.list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_standing_list(&s, &ctx) }
            })),
        );
    }
    {
        let s = agent_store.clone();
        bridge.register(
            "agent.standing_approval.revoke",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_standing_revoke(&s, &ctx) }
            })),
        );
    }

    // Wire the agent gate itself. The describe closure
    // returns an empty descriptor on the coordinator — the
    // coordinator's *own* capabilities aren't categorised in
    // a way that would change the gate's decision. The
    // on_require_approval closure mints the approval row
    // synchronously.
    let bindings_store = agent_store.clone();
    let bindings_create = agent_store.clone();
    let bindings_task_store = task_store.clone();
    // SEC PART 7: back the agent gate's per-request descriptor
    // lookup with the shared `DescriptorCache` the manifest
    // populates at registration time. The closure is a single
    // lock-free `DashMap::get` per call — replaces the pre-fix
    // stub that always returned `None` and forced the gate to
    // fall through to a category-free / risk-free admit.
    bridge.set_agent_gate(crate::dispatch::AgentGateBindings {
        store: bindings_store,
        describe: crate::dispatch::describe_fn_from_cache(descriptor_cache),
        on_require_approval: Arc::new(move |req, _task_id_hint| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let expires_at = now + req.approval_timeout_secs;
            // BLAKE3 hash of the request method as a placeholder
            // for args_redacted_hash — the bridge would normally
            // salt with request_id, but the gate doesn't have
            // the raw args at this point and re-hashing twice
            // would defeat the redaction guarantee. We stamp
            // the method so operators can correlate.
            let hash = hex::encode(blake3::hash(req.method.as_bytes()).as_bytes());
            let task_id = req.task_id.as_deref();
            let approval_id = bindings_create
                .create_approval(
                    &req.agent_id,
                    &req.subject_id,
                    &req.method,
                    &req.category,
                    &hash,
                    &req.reason,
                    &req.approver_groups,
                    task_id,
                    expires_at,
                    // DEFERRED 2: the gate already read the
                    // operator-allow-list out of the agent
                    // profile (`GateApprovalRequest::authorized_approvers`);
                    // stamp it onto the row so
                    // `coord.approval.decide` can enforce the
                    // check against the caller's subject id.
                    &req.authorized_approvers,
                    // GROUP 6: stamp the request's VERIFIED tenant
                    // onto the approval_requests row.
                    &req.tenant_id,
                )
                .map_err(|e| e.to_string())?;
            // When the caller threaded a task_id through the
            // envelope, flip it to awaiting_input and append a
            // chronicle event so the dashboard / SOL flow
            // polling task.get sees the pause. The
            // `coord.approval.decide` handler later resumes
            // (approved) or fails (rejected) the same task by
            // reading the row's `task_id` column.
            if let Some(tid) = task_id {
                if let Err(e) = bindings_task_store.update(
                    tid,
                    Some("awaiting_input"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ) {
                    tracing::warn!(
                        task_id = %tid,
                        approval_id = %approval_id,
                        error = %e,
                        "on_require_approval: task awaiting_input flip failed"
                    );
                }
                let payload = format!("approval_id={approval_id}|method={}", req.method);
                if let Err(e) =
                    bindings_task_store.append_event(tid, "task.approval_requested", &payload)
                {
                    tracing::warn!(
                        task_id = %tid,
                        error = %e,
                        "on_require_approval: chronicle event failed"
                    );
                }
            }
            Ok(approval_id)
        }),
    });
}

async fn populate_delegation_ai_cell(
    cell: crate::nodes::coordinator::delegate::DelegationAiDispatcherCell,
    cfg: crate::nodes::coordinator::delegate::DelegationAiPeerConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "delegation executor: identity bundle missing; AI dispatcher disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "delegation executor: identity bundle decode failed; AI dispatcher disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "delegation executor: client key not 32 bytes; AI dispatcher disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "delegation executor: client key missing; AI dispatcher disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.alias.clone(),
        PeerEntry {
            addr: cfg.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                alias = %cfg.alias,
                addr = %cfg.addr,
                "delegation executor: discover_and_pin returned None; AI dispatcher disabled"
            );
            return;
        }
    };
    let dispatcher: Arc<dyn crate::nodes::coordinator::delegate::DelegationAiDispatcher> = Arc::new(
        crate::nodes::coordinator::delegate::DelegationAiMeshDispatcher::new(
            mesh,
            cfg.alias.clone(),
            bundle,
            cfg.deadline_secs,
        ),
    );
    if cell.set(dispatcher).is_err() {
        tracing::warn!("delegation executor: AI cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            alias = %cfg.alias,
            addr = %cfg.addr,
            "coordinator node: delegation AI dispatcher online"
        );
    }
}

async fn populate_cron_ai_cell(
    cell: crate::nodes::coordinator::cron::CronAiDispatcherCell,
    cfg: crate::nodes::coordinator::cron::CronAiPeerConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "cron scheduler: identity bundle missing; AI dispatcher disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cron scheduler: identity bundle decode failed; AI dispatcher disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "cron scheduler: client key not 32 bytes; AI dispatcher disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "cron scheduler: client key missing; AI dispatcher disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.alias.clone(),
        PeerEntry {
            addr: cfg.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                alias = %cfg.alias,
                addr = %cfg.addr,
                "cron scheduler: discover_and_pin returned None; AI dispatcher disabled"
            );
            return;
        }
    };
    let dispatcher: Arc<dyn crate::nodes::coordinator::cron::CronAiDispatcher> =
        Arc::new(crate::nodes::coordinator::cron::CronAiMeshDispatcher::new(
            mesh,
            cfg.alias.clone(),
            bundle,
            cfg.deadline_secs,
        ));
    if cell.set(dispatcher).is_err() {
        tracing::warn!("cron scheduler: AI cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            alias = %cfg.alias,
            addr = %cfg.addr,
            "coordinator node: cron AI dispatcher online"
        );
    }
}

async fn populate_telegram_outbound_cell(
    cell: crate::nodes::telegram::TelegramOutboundClientCell,
    cfg: crate::nodes::telegram::TelegramNodeConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "telegram: identity bundle missing; outbound mesh client disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "telegram: identity bundle decode failed; outbound mesh client disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "telegram: client key not 32 bytes; outbound mesh client disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "telegram: client key missing; outbound mesh client disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.memory_peer.alias.clone(),
        PeerEntry {
            addr: cfg.memory_peer.addr.clone(),
        },
    );
    peers_map.insert(
        cfg.ai_peer.alias.clone(),
        PeerEntry {
            addr: cfg.ai_peer.addr.clone(),
        },
    );
    peers_map.insert(
        cfg.coord_peer.alias.clone(),
        PeerEntry {
            addr: cfg.coord_peer.addr.clone(),
        },
    );
    if let Some(audio) = &cfg.audio_peer {
        peers_map.insert(
            audio.alias.clone(),
            PeerEntry {
                addr: audio.addr.clone(),
            },
        );
    }
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        // Use the AI deadline as the outer bound — it's the
        // longest of the three configured per-call deadlines
        // for typical configs.
        deadline_secs: cfg.ai_peer.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                memory = %cfg.memory_peer.addr,
                ai = %cfg.ai_peer.addr,
                coord = %cfg.coord_peer.addr,
                "telegram: discover_and_pin returned None; outbound client disabled"
            );
            return;
        }
    };

    let audio_alias = cfg.audio_peer.as_ref().map(|p| p.alias.clone());
    let audio_deadline_secs = cfg
        .audio_peer
        .as_ref()
        .map(|p| p.deadline_secs)
        .unwrap_or(90);
    let client = Arc::new(crate::nodes::telegram::TelegramOutboundClient {
        mesh,
        identity: bundle,
        memory_alias: cfg.memory_peer.alias.clone(),
        memory_deadline_secs: cfg.memory_peer.deadline_secs,
        ai_alias: cfg.ai_peer.alias.clone(),
        ai_deadline_secs: cfg.ai_peer.deadline_secs,
        coord_alias: cfg.coord_peer.alias.clone(),
        coord_deadline_secs: cfg.coord_peer.deadline_secs,
        audio_alias,
        audio_deadline_secs,
    });
    if cell.set(client).is_err() {
        tracing::warn!("telegram: outbound cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            memory = %cfg.memory_peer.alias,
            ai = %cfg.ai_peer.alias,
            coord = %cfg.coord_peer.alias,
            "telegram node: outbound mesh client online"
        );
    }
}

async fn populate_discord_outbound_cell(
    cell: crate::nodes::discord::DiscordOutboundClientCell,
    cfg: crate::nodes::discord::DiscordNodeConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "discord: identity bundle missing; outbound mesh client disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "discord: identity bundle decode failed; outbound mesh client disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "discord: client key not 32 bytes; outbound mesh client disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "discord: client key missing; outbound mesh client disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.memory_peer.alias.clone(),
        PeerEntry {
            addr: cfg.memory_peer.addr.clone(),
        },
    );
    peers_map.insert(
        cfg.ai_peer.alias.clone(),
        PeerEntry {
            addr: cfg.ai_peer.addr.clone(),
        },
    );
    peers_map.insert(
        cfg.coord_peer.alias.clone(),
        PeerEntry {
            addr: cfg.coord_peer.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.ai_peer.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                memory = %cfg.memory_peer.addr,
                ai = %cfg.ai_peer.addr,
                coord = %cfg.coord_peer.addr,
                "discord: discover_and_pin returned None; outbound client disabled"
            );
            return;
        }
    };

    let client = Arc::new(crate::nodes::discord::DiscordOutboundClient {
        mesh,
        identity: bundle,
        memory_alias: cfg.memory_peer.alias.clone(),
        memory_deadline_secs: cfg.memory_peer.deadline_secs,
        ai_alias: cfg.ai_peer.alias.clone(),
        ai_deadline_secs: cfg.ai_peer.deadline_secs,
        coord_alias: cfg.coord_peer.alias.clone(),
        coord_deadline_secs: cfg.coord_peer.deadline_secs,
    });
    if cell.set(client).is_err() {
        tracing::warn!("discord: outbound cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            memory = %cfg.memory_peer.alias,
            ai = %cfg.ai_peer.alias,
            coord = %cfg.coord_peer.alias,
            "discord node: outbound mesh client online"
        );
    }
}

async fn populate_slack_outbound_cell(
    cell: crate::nodes::slack::SlackOutboundClientCell,
    cfg: crate::nodes::slack::SlackNodeConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "slack: identity bundle missing; outbound mesh client disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "slack: identity bundle decode failed; outbound mesh client disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "slack: client key not 32 bytes; outbound mesh client disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "slack: client key missing; outbound mesh client disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.memory_peer.alias.clone(),
        PeerEntry {
            addr: cfg.memory_peer.addr.clone(),
        },
    );
    peers_map.insert(
        cfg.ai_peer.alias.clone(),
        PeerEntry {
            addr: cfg.ai_peer.addr.clone(),
        },
    );
    peers_map.insert(
        cfg.coord_peer.alias.clone(),
        PeerEntry {
            addr: cfg.coord_peer.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.ai_peer.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                memory = %cfg.memory_peer.addr,
                ai = %cfg.ai_peer.addr,
                coord = %cfg.coord_peer.addr,
                "slack: discover_and_pin returned None; outbound client disabled"
            );
            return;
        }
    };

    let client = Arc::new(crate::nodes::slack::SlackOutboundClient {
        mesh,
        identity: bundle,
        memory_alias: cfg.memory_peer.alias.clone(),
        memory_deadline_secs: cfg.memory_peer.deadline_secs,
        ai_alias: cfg.ai_peer.alias.clone(),
        ai_deadline_secs: cfg.ai_peer.deadline_secs,
        coord_alias: cfg.coord_peer.alias.clone(),
        coord_deadline_secs: cfg.coord_peer.deadline_secs,
    });
    if cell.set(client).is_err() {
        tracing::warn!("slack: outbound cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            memory = %cfg.memory_peer.alias,
            ai = %cfg.ai_peer.alias,
            coord = %cfg.coord_peer.alias,
            "slack node: outbound mesh client online"
        );
    }
}

/// Email-channel outbound wiring. Same shape as the slack /
/// discord / telegram populate functions — dials memory + ai +
/// coord peers, builds an `EmailOutboundClient`, publishes into
/// the cell. The IMAP listener loop already runs; on first
/// inbound message the controller reads the cell and routes
/// through whichever peers are reachable.
async fn populate_email_outbound_cell(
    cell: crate::nodes::email::EmailOutboundClientCell,
    cfg: crate::nodes::email::EmailNodeConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "email: identity bundle missing; outbound mesh client disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "email: identity bundle decode failed; outbound mesh client disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "email: client key not 32 bytes; outbound mesh client disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "email: client key missing; outbound mesh client disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.memory_peer.alias.clone(),
        PeerEntry {
            addr: cfg.memory_peer.addr.clone(),
        },
    );
    peers_map.insert(
        cfg.ai_peer.alias.clone(),
        PeerEntry {
            addr: cfg.ai_peer.addr.clone(),
        },
    );
    peers_map.insert(
        cfg.coord_peer.alias.clone(),
        PeerEntry {
            addr: cfg.coord_peer.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.ai_peer.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                memory = %cfg.memory_peer.addr,
                ai = %cfg.ai_peer.addr,
                coord = %cfg.coord_peer.addr,
                "email: discover_and_pin returned None; outbound client disabled"
            );
            return;
        }
    };

    let client = Arc::new(crate::nodes::email::EmailOutboundClient {
        mesh,
        identity: bundle,
        memory_alias: cfg.memory_peer.alias.clone(),
        memory_deadline_secs: cfg.memory_peer.deadline_secs,
        ai_alias: cfg.ai_peer.alias.clone(),
        ai_deadline_secs: cfg.ai_peer.deadline_secs,
        coord_alias: cfg.coord_peer.alias.clone(),
        coord_deadline_secs: cfg.coord_peer.deadline_secs,
    });
    if cell.set(client).is_err() {
        tracing::warn!("email: outbound cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            memory = %cfg.memory_peer.alias,
            ai = %cfg.ai_peer.alias,
            coord = %cfg.coord_peer.alias,
            "email node: outbound mesh client online"
        );
    }
}

async fn populate_memory_curator_cell(
    cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::AiDispatcher>>>,
    state: Arc<tokio::sync::Mutex<crate::nodes::memory::CuratorState>>,
    cfg: crate::nodes::memory::AiPeerConfig,
    key_path: std::path::PathBuf,
    interval_secs: u64,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "memory curator: identity bundle missing; AI dispatcher disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "memory curator: identity bundle decode failed; AI dispatcher disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "memory curator: client key not 32 bytes; AI dispatcher disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "memory curator: client key missing; AI dispatcher disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.alias.clone(),
        PeerEntry {
            addr: cfg.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                alias = %cfg.alias,
                addr = %cfg.addr,
                "memory curator: discover_and_pin returned None; AI dispatcher disabled"
            );
            return;
        }
    };
    let dispatcher: Arc<dyn crate::nodes::memory::AiDispatcher> =
        Arc::new(crate::nodes::memory::AiMeshDispatcher::new(
            mesh,
            cfg.alias.clone(),
            bundle,
            cfg.deadline_secs,
        ));
    if cell.set(dispatcher).is_err() {
        tracing::warn!("memory curator: cell already populated; spurious second wiring");
    } else {
        // Stamp the initial next_run_at so /v1/memory/curator/
        // status reports it even before the first tick lands.
        {
            let mut guard = state.lock().await;
            guard.next_run_at = Some(unix_now() + interval_secs as i64);
        }
        tracing::info!(
            alias = %cfg.alias,
            addr = %cfg.addr,
            interval_secs = interval_secs,
            "memory node: curator dispatcher online; scheduler ticking"
        );
    }
}

/// Mirror of `populate_memory_curator_cell` for the coord-peer
/// dispatcher. When `[memory.curator.coord_peer]` is set, the
/// memory controller dials the coordinator post-startup and
/// publishes a `CoordMeshDispatcher` into the cell so the
/// curator scheduler can write `memory.curator_run` chronicle
/// events after every tick.
///
/// Same silent-failure posture as the AI dispatcher path —
/// missing bundle / bad key / discover failure all surface as
/// a single WARN and the cell stays empty (the scheduler then
/// logs one WARN per tick and skips the chronicle write).
async fn populate_memory_curator_coord_cell(
    cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::CoordDispatcher>>>,
    cfg: crate::nodes::memory::CoordPeerConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "memory curator coord: identity bundle missing; chronicle events disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "memory curator coord: identity bundle decode failed; chronicle events disabled"
            );
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "memory curator coord: client key not 32 bytes; chronicle events disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                key_path = %key_path.display(),
                error = %e,
                "memory curator coord: client key missing; chronicle events disabled"
            );
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.alias.clone(),
        PeerEntry {
            addr: cfg.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
        source_key_registry: None,
    };

    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                alias = %cfg.alias,
                addr = %cfg.addr,
                "memory curator coord: discover_and_pin returned None; chronicle events disabled"
            );
            return;
        }
    };
    let dispatcher: Arc<dyn crate::nodes::memory::CoordDispatcher> =
        Arc::new(crate::nodes::memory::CoordMeshDispatcher::new(
            mesh,
            cfg.alias.clone(),
            bundle,
            cfg.deadline_secs,
        ));
    if cell.set(dispatcher).is_err() {
        tracing::warn!("memory curator coord: cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            alias = %cfg.alias,
            addr = %cfg.addr,
            "memory node: coordinator dispatcher online; chronicle events enabled"
        );
    }
}

/// Dial the AI peer named in `[memory.embedding_peer]` and
/// populate the embedding-dispatcher cell so memory.embed /
/// memory.search / memory.embed_all can route through it.
/// W4: build a `MeshDriftEmbedDispatcher` pointed at the
/// operator-configured AI peer and publish it into the
/// coordinator's drift-embedder cell. Mirrors
/// `populate_memory_embedding_cell` so identity / discovery /
/// retry semantics stay consistent across mesh-using
/// dispatchers.
async fn populate_drift_embedder_cell(
    cell: crate::nodes::ai::guardrails::DriftEmbedDispatcherCell,
    cfg: crate::nodes::coordinator::CoordinatorAiPeerConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};
    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "drift embedder: identity bundle missing; dispatcher disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "drift embedder: bundle decode failed");
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        _ => {
            tracing::warn!(
                key_path = %key_path.display(),
                "drift embedder: client key missing / wrong length"
            );
            return;
        }
    };
    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.alias.clone(),
        PeerEntry {
            addr: cfg.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };
    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        source_key_registry: None,
    };
    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                addr = %cfg.addr,
                "drift embedder: discover_and_pin returned None; dispatcher disabled"
            );
            return;
        }
    };
    let dispatcher: Arc<dyn crate::nodes::ai::guardrails::DriftEmbedDispatcher> =
        Arc::new(crate::nodes::ai::guardrails::MeshDriftEmbedDispatcher::new(
            mesh,
            cfg.alias.clone(),
            bundle,
            cfg.deadline_secs,
        ));
    if cell.set(dispatcher).is_err() {
        tracing::warn!("drift embedder: cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            alias = %cfg.alias,
            addr = %cfg.addr,
            "coordinator: drift embedder dispatcher online (W4 live)"
        );
    }
}

/// Build a `MeshWorkflowDispatcher` pointed at every configured
/// peer and publish it into the coordinator's workflow
/// dispatcher cell. Failure (missing identity bundle, no
/// peers, discovery timeout) is non-fatal — the cell stays
/// empty and `workflow.run` returns a "dispatcher not ready"
/// error instead of panicking.
async fn populate_workflow_dispatcher_cell(
    cell: crate::workflow::WorkflowDispatcherCell,
    peers: std::collections::BTreeMap<String, PeerConfig>,
    key_path: std::path::PathBuf,
    deadline_secs: i64,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};
    if peers.is_empty() {
        tracing::info!(
            "workflow dispatcher: no [peers] configured; mesh dispatcher disabled (workflow.run will return a clear error)"
        );
        return;
    }
    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "workflow dispatcher: identity bundle missing; dispatcher disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "workflow dispatcher: bundle decode failed");
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        _ => {
            tracing::warn!(
                key_path = %key_path.display(),
                "workflow dispatcher: client key missing / wrong length"
            );
            return;
        }
    };
    let mut peers_map = std::collections::HashMap::new();
    for (alias, peer_cfg) in &peers {
        peers_map.insert(
            alias.clone(),
            PeerEntry {
                addr: format!("/ip4/127.0.0.1/tcp/{}", peer_cfg.port),
            },
        );
    }
    let peers_file = PeersFile { peers: peers_map };
    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        source_key_registry: None,
    };
    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                "workflow dispatcher: discover_and_pin returned None; dispatcher disabled"
            );
            return;
        }
    };
    let dispatcher: std::sync::Arc<dyn crate::workflow::WorkflowDispatcher> = std::sync::Arc::new(
        crate::workflow::MeshWorkflowDispatcher::new(mesh, bundle, deadline_secs),
    );
    if cell.set(dispatcher).is_err() {
        tracing::warn!("workflow dispatcher: cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            peer_count = peers.len(),
            "coordinator: workflow dispatcher online (RELIX-7.5)"
        );
    }
}

/// RELIX-7.16 GAP 3: build one
/// `MeshKnowledgeDispatcher` per configured peer and publish a
/// `MeshKnowledgeRouter` into `cell` so cross-node shares stop
/// rejecting with `Unreachable`. Peer alias === node name in
/// `[[knowledge.groups.member_nodes]]`, so the mapping is
/// 1:1 from `[peers]`. Silent failure — when the identity
/// bundle or key file is missing the mesh stays disabled and
/// cross-node shares continue to reject.
async fn populate_knowledge_mesh_cell(
    cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::knowledge::RemoteKnowledgeDispatcher>>>,
    peers: std::collections::BTreeMap<String, PeerConfig>,
    key_path: std::path::PathBuf,
    deadline_secs: i64,
    source_key_registry: crate::knowledge::service::SourceNodeKeyRegistry,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};
    if peers.is_empty() {
        tracing::info!("knowledge mesh: no [peers] configured; cross-node shares disabled");
        return;
    }
    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "knowledge mesh: identity bundle missing; cross-node shares disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "knowledge mesh: bundle decode failed");
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        _ => {
            tracing::warn!(
                key_path = %key_path.display(),
                "knowledge mesh: client key missing / wrong length"
            );
            return;
        }
    };
    let mut peers_map = std::collections::HashMap::new();
    for (alias, peer_cfg) in &peers {
        peers_map.insert(
            alias.clone(),
            PeerEntry {
                addr: format!("/ip4/127.0.0.1/tcp/{}", peer_cfg.port),
            },
        );
    }
    let peers_file = PeersFile { peers: peers_map };
    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        // SEC §17: discovery auto-registers each peer's
        // handshake-verified identity key here, so knowledge-share
        // source binding works with zero manual config.
        source_key_registry: Some(source_key_registry),
    };
    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!("knowledge mesh: discover_and_pin returned None; disabled");
            return;
        }
    };
    let mut by_node: std::collections::BTreeMap<
        String,
        Arc<crate::knowledge::MeshKnowledgeDispatcher>,
    > = std::collections::BTreeMap::new();
    for alias in peers.keys() {
        let d = Arc::new(crate::knowledge::MeshKnowledgeDispatcher::new(
            mesh.clone(),
            alias.clone(),
            bundle.clone(),
            deadline_secs,
        ));
        by_node.insert(alias.clone(), d);
    }
    let router: Arc<dyn crate::knowledge::RemoteKnowledgeDispatcher> =
        Arc::new(crate::knowledge::MeshKnowledgeRouter::new(by_node));
    if cell.set(router).is_err() {
        tracing::warn!("knowledge mesh: cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            peer_count = peers.len(),
            "knowledge mesh: online (RELIX-7.16 GAP 3)"
        );
    }
}

/// RELIX-7.11 GAP 3: populate the alert-fan-out cell with an
/// `AlertMeshContext` once the rpc::Client is up. The
/// MultiChannelAlertSink reads from the cell on every alert; an
/// empty cell means alerts get logged + chronicled but the
/// channel fan-out skips (logged at warn).
async fn populate_alert_mesh_cell(
    cell: crate::metrics::AlertMeshCell,
    peers: std::collections::BTreeMap<String, PeerConfig>,
    key_path: std::path::PathBuf,
    deadline_secs: i64,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};
    if peers.is_empty() {
        tracing::info!(
            "alert mesh: no [peers] configured; channel fan-out disabled (chronicle still records)"
        );
        return;
    }
    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "alert mesh: identity bundle missing; channel fan-out disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "alert mesh: bundle decode failed");
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        _ => {
            tracing::warn!(
                key_path = %key_path.display(),
                "alert mesh: client key missing / wrong length"
            );
            return;
        }
    };
    let mut peers_map = std::collections::HashMap::new();
    for (alias, peer_cfg) in &peers {
        peers_map.insert(
            alias.clone(),
            PeerEntry {
                addr: format!("/ip4/127.0.0.1/tcp/{}", peer_cfg.port),
            },
        );
    }
    let peers_file = PeersFile { peers: peers_map };
    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        source_key_registry: None,
    };
    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!("alert mesh: discover_and_pin returned None; channel fan-out disabled");
            return;
        }
    };
    let ctx = crate::metrics::AlertMeshContext {
        mesh,
        identity: bundle,
    };
    if cell.set(ctx).is_err() {
        tracing::warn!("alert mesh: cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            peer_count = peers.len(),
            "coordinator: alert mesh online (RELIX-7.11 GAP 3 / 4)"
        );
    }
}

/// Mirrors `populate_memory_curator_cell` — same identity-bundle
/// + client-key + discover_and_pin pattern.
async fn populate_memory_embedding_cell(
    cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::EmbeddingDispatcher>>>,
    cfg: crate::nodes::memory::EmbeddingPeerConfig,
    key_path: std::path::PathBuf,
) {
    use crate::flow_runner::{PeerEntry, PeersFile};
    use crate::manifest::{DiscoveryOptions, discover_and_pin};

    let bundle_path = key_path.with_extension("bundle");
    let bundle_bytes = match std::fs::read(&bundle_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                bundle_path = %bundle_path.display(),
                error = %e,
                "memory embedding: identity bundle missing; dispatcher disabled"
            );
            return;
        }
    };
    let bundle: relix_core::bundle::Bundle = match relix_core::codec::decode(&bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "memory embedding: bundle decode failed");
            return;
        }
    };
    let client_key_bytes = match std::fs::read(&key_path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            k
        }
        Ok(_) => {
            tracing::warn!(
                key_path = %key_path.display(),
                "memory embedding: client key not 32 bytes; dispatcher disabled"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "memory embedding: client key missing");
            return;
        }
    };

    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        cfg.alias.clone(),
        PeerEntry {
            addr: cfg.addr.clone(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };

    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key_bytes),
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
        source_key_registry: None,
    };
    let (_cache, mesh) = match discover_and_pin(opts).await {
        Some(p) => p,
        None => {
            tracing::warn!(
                addr = %cfg.addr,
                "memory embedding: discover_and_pin returned None; dispatcher disabled"
            );
            return;
        }
    };

    let dispatcher: Arc<dyn crate::nodes::memory::EmbeddingDispatcher> =
        Arc::new(crate::nodes::memory::EmbeddingMeshDispatcher::new(
            mesh,
            cfg.alias.clone(),
            bundle,
            cfg.deadline_secs,
        ));
    if cell.set(dispatcher).is_err() {
        tracing::warn!("memory embedding: cell already populated; spurious second wiring");
    } else {
        tracing::info!(
            alias = %cfg.alias,
            model = %cfg.model,
            "memory node: embedding dispatcher online"
        );
    }
}

/// Register `plugin.list`, `plugin.status`, `plugin.reload`,
/// `plugin.disable` on the supplied dispatch bridge. Shared
/// state (`PluginHostState`) carries the registry + the
/// in-memory map of currently-loaded plugins so reload / disable
/// can act on the live subprocess.
fn register_plugin_management_capabilities(
    bridge: &mut DispatchBridge,
    state: crate::plugin::PluginHostState,
) {
    use crate::dispatch::{FnHandler, HandlerOutcome, InvocationCtx};
    use crate::plugin::PluginStatus;
    use relix_core::types::{ErrorEnvelope, error_kinds};

    // Each management cap is registered under TWO names:
    //   - the bare "plugin.list" / "plugin.status" / "plugin.reload"
    //     / "plugin.disable" — direct ping, SOL `remote_call`, and
    //     the bridge HTTP routes use these,
    //   - the peer-prefixed "plugin_host.plugin.list" etc. — what
    //     .sflow's `step y: plugin_host.plugin.list ""` arrives as
    //     on the wire, since sflow's wire_method carries the peer
    //     prefix the user typed.
    {
        let state = state.clone();
        let handler: Arc<dyn crate::dispatch::Handler> =
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let rows = match state.registry.list() {
                        Ok(r) => r,
                        Err(e) => {
                            return HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::RESPONDER_INTERNAL,
                                cause: format!("plugin.list: {e}"),
                                retry_hint: 1,
                                retry_after: None,
                            });
                        }
                    };
                    let mut body = String::new();
                    for r in &rows {
                        body.push_str(&format!(
                            "{}\t{}\t{}\t{}\t{}\n",
                            r.plugin_id,
                            r.name,
                            r.version,
                            r.status.as_wire(),
                            r.capabilities.len()
                        ));
                    }
                    body.push_str(&format!("count={}\n", rows.len()));
                    HandlerOutcome::Ok(body.into_bytes())
                }
            }));
        bridge.register("plugin.list", handler.clone());
        bridge.register("plugin_host.plugin.list", handler);
    }
    {
        let state = state.clone();
        let handler: Arc<dyn crate::dispatch::Handler> = Arc::new(FnHandler(
            move |ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let plugin_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                    if plugin_id.is_empty() {
                        return HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: "plugin.status: plugin_id required".into(),
                            retry_hint: 2,
                            retry_after: None,
                        });
                    }
                    let row = match state.registry.get(&plugin_id) {
                        Ok(Some(r)) => r,
                        Ok(None) => {
                            return HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::INVALID_ARGS,
                                cause: format!("plugin.status: not found: {plugin_id}"),
                                retry_hint: 2,
                                retry_after: None,
                            });
                        }
                        Err(e) => {
                            return HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::RESPONDER_INTERNAL,
                                cause: format!("plugin.status: {e}"),
                                retry_hint: 1,
                                retry_after: None,
                            });
                        }
                    };
                    let caps = row.capabilities.join(",");
                    let last_seen = row
                        .last_seen_at
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "-1".to_string());
                    let body = format!(
                        "plugin_id={}|name={}|version={}|status={}|registered_at={}|last_seen_at={}|capabilities={}|node_type={}|error_message={}\n",
                        row.plugin_id,
                        row.name,
                        row.version,
                        row.status.as_wire(),
                        row.registered_at,
                        last_seen,
                        caps,
                        row.node_type,
                        row.error_message,
                    );
                    HandlerOutcome::Ok(body.into_bytes())
                }
            },
        ));
        bridge.register("plugin.status", handler.clone());
        bridge.register("plugin_host.plugin.status", handler);
    }
    {
        let state = state.clone();
        let handler: Arc<dyn crate::dispatch::Handler> =
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let plugin_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                    if plugin_id.is_empty() {
                        return HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: "plugin.reload: plugin_id required".into(),
                            retry_hint: 2,
                            retry_after: None,
                        });
                    }
                    let row = match state.registry.get(&plugin_id) {
                        Ok(Some(r)) => r,
                        Ok(None) => {
                            return HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::INVALID_ARGS,
                                cause: format!("plugin.reload: not found: {plugin_id}"),
                                retry_hint: 2,
                                retry_after: None,
                            });
                        }
                        Err(e) => {
                            return HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::RESPONDER_INTERNAL,
                                cause: format!("plugin.reload: {e}"),
                                retry_hint: 1,
                                retry_after: None,
                            });
                        }
                    };
                    // Shutdown the existing subprocess.
                    let existing = {
                        let mut map = state.plugins.write().await;
                        map.remove(&plugin_id)
                    };
                    if let Some(p) = existing {
                        p.shutdown().await;
                    }
                    // Re-spawn from the same manifest path.
                    let path = std::path::PathBuf::from(&row.manifest_path);
                    let manifest = match crate::plugin::PluginManifest::load_from_path(&path) {
                        Ok(m) => m,
                        Err(e) => {
                            let msg = format!("plugin.reload: re-parse: {e}");
                            let _ = state.registry.set_status(
                                &plugin_id,
                                PluginStatus::Error,
                                Some(&msg),
                            );
                            return HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::RESPONDER_INTERNAL,
                                cause: msg,
                                retry_hint: 1,
                                retry_after: None,
                            });
                        }
                    };
                    // SEC PART 2: plugin reload uses the same
                    // sandbox limits as initial spawn. Defaults
                    // until per-tenant override is wired.
                    match crate::plugin::PluginLoader::spawn(
                        manifest,
                        path,
                        10,
                        30,
                        crate::plugin::SandboxLimits::default(),
                    )
                    .await
                    {
                        Ok(loaded) => {
                            let _ = state.registry.set_status(
                                &loaded.plugin_id,
                                PluginStatus::Active,
                                None,
                            );
                            let _ = state.registry.touch(&loaded.plugin_id);
                            state
                                .plugins
                                .write()
                                .await
                                .insert(loaded.plugin_id.clone(), loaded);
                            HandlerOutcome::Ok(b"ok\n".to_vec())
                        }
                        Err(e) => {
                            let msg = format!("plugin.reload: spawn: {e}");
                            let _ = state.registry.set_status(
                                &plugin_id,
                                PluginStatus::Error,
                                Some(&msg),
                            );
                            HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::RESPONDER_INTERNAL,
                                cause: msg,
                                retry_hint: 1,
                                retry_after: None,
                            })
                        }
                    }
                }
            }));
        bridge.register("plugin.reload", handler.clone());
        bridge.register("plugin_host.plugin.reload", handler);
    }
    {
        let state = state.clone();
        let handler: Arc<dyn crate::dispatch::Handler> =
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let plugin_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                    if plugin_id.is_empty() {
                        return HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: "plugin.disable: plugin_id required".into(),
                            retry_hint: 2,
                            retry_after: None,
                        });
                    }
                    if state.registry.get(&plugin_id).ok().flatten().is_none() {
                        return HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: format!("plugin.disable: not found: {plugin_id}"),
                            retry_hint: 2,
                            retry_after: None,
                        });
                    }
                    let existing = state.plugins.write().await.remove(&plugin_id);
                    if let Some(p) = existing {
                        p.shutdown().await;
                    }
                    let _ = state
                        .registry
                        .set_status(&plugin_id, PluginStatus::Disabled, None);
                    HandlerOutcome::Ok(b"ok\n".to_vec())
                }
            }));
        bridge.register("plugin.disable", handler.clone());
        bridge.register("plugin_host.plugin.disable", handler);
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Productized **review-to-done** for `run.apply` (company-model §12.5B /
/// §12.6). A clean, accept-gated operator apply IS the operator's
/// review-to-done, so the run's Brief should reach board `done` — which
/// resolves every dependent's blocker — WITHOUT a separate manual
/// `brief.move done`. (`run.apply` already requires the run to be `done` +
/// `accepted`, so reaching here means the operator deliberately accepted and
/// applied; the Shift itself never auto-advances the board.)
///
/// Narrow + honest: it advances ONLY a Brief that is genuinely awaiting review
/// (`in_review`) — any other column (already `done`, re-opened, `cancelled`)
/// is left untouched. Best-effort: a board-move refusal NEVER fails the apply
/// (the files are already applied) — it leaves the Brief in review and returns
/// `None`. Returns the Brief's resulting board status when it advanced, so the
/// caller can report the board change honestly.
fn advance_reviewed_brief(
    store: &crate::nodes::coordinator::TaskStore,
    brief_id: &str,
    run_id: &str,
) -> Option<String> {
    match store.complete_reviewed_brief(brief_id) {
        Ok(Some(to)) => {
            let _ = store.append_run_event(
                run_id,
                "apply.brief_done",
                "relix",
                &format!("review-to-done: Brief {brief_id} → {to} (dependents unblock)"),
                None,
                false,
            );
            Some(to)
        }
        // Not awaiting review (already done / re-opened / cancelled) — honest no-op.
        Ok(None) => None,
        // A board refusal must never fail an already-successful apply.
        Err(_) => None,
    }
}

/// The mesh-independent body of the `run.diff` capability — the safe-apply
/// PLAN preview (`GET /v1/runs/:id/diff`). PURE: never mutates files or the
/// ledger. Reports per-file action / conflict plus whether the run is
/// apply-eligible (done + accepted + scoped workspace) — and the plan is
/// computed regardless of eligibility, so an operator can preview the pending
/// change BEFORE accepting. Tenant-scoped: another Guild's run reads not-found.
///
/// Returns the JSON response body, or an `INVALID_ARGS` cause string on
/// refusal/error. Extracted from the capability closure so the live route and
/// its in-process integration tests run the SAME code. `pub` so the bridge's
/// mini-mesh HTTP regression can register the SAME body behind a fake
/// coordinator peer and prove the route/serialization hop end to end.
pub fn execute_run_diff(
    store: &crate::nodes::coordinator::TaskStore,
    run_id: &str,
    tenant: &str,
) -> Result<serde_json::Value, String> {
    match store.run_belongs_to_tenant(run_id, tenant) {
        Ok(true) => {}
        Ok(false) => return Err(format!("run not found: {run_id}")),
        Err(e) => return Err(format!("run.diff: {e}")),
    }
    let run = match store.get_run(run_id) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(format!("run not found: {run_id}")),
        Err(e) => return Err(format!("run.diff: {e}")),
    };
    let eligibility = crate::nodes::coordinator::heartbeat::run_apply_eligibility(&run);
    let eligible = eligibility.is_ok();
    let reason = match &eligibility {
        Ok(()) => "eligible".to_string(),
        Err(e) => e.clone(),
    };
    let artifacts = match store.list_run_artifacts(run_id) {
        Ok(a) => a,
        Err(e) => return Err(format!("run.diff: {e}")),
    };
    let root = store.run_workspace_config().project_root.clone();
    let plan = match crate::nodes::coordinator::heartbeat::build_apply_plan(&root, &artifacts) {
        Ok(p) => p,
        Err(e) => return Err(format!("run.diff: invalid project root: {e}")),
    };
    Ok(serde_json::json!({
        "run_id": run_id,
        "status": run.status,
        "review": run.review,
        "apply_status": run.apply_status,
        "eligible": eligible,
        "reason": reason,
        "plan": plan,
    }))
}

/// The mesh-independent body of the `run.apply` capability — apply an accepted
/// run's changed files back into the configured project root (`POST
/// /v1/runs/:id/apply`). Refuses the WHOLE apply if ANY file is unsafe /
/// conflicted (no partial apply, no `force`). Tenant-scoped; only a `done` +
/// `accepted` + scoped-workspace run applies. Records the durable apply status
/// and the apply.plan / apply.started / apply.applied / apply.conflicted /
/// apply.failed transcript events, and — ONLY on a clean `applied` — closes the
/// productized review-to-done (advances the Brief to `done`, unblocking
/// dependents).
///
/// Returns the JSON response body, or an `INVALID_ARGS` cause string on
/// refusal/error. ALL durable side effects happen here so the live route and
/// its in-process integration tests run the SAME code. `pub` so the bridge's
/// mini-mesh HTTP regression can register the SAME body behind a fake
/// coordinator peer and prove the route/serialization hop end to end.
pub fn execute_run_apply(
    store: &crate::nodes::coordinator::TaskStore,
    run_id: &str,
    tenant: &str,
) -> Result<serde_json::Value, String> {
    match store.run_belongs_to_tenant(run_id, tenant) {
        Ok(true) => {}
        Ok(false) => return Err(format!("run not found: {run_id}")),
        Err(e) => return Err(format!("run.apply: {e}")),
    }
    let run = match store.get_run(run_id) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(format!("run not found: {run_id}")),
        Err(e) => return Err(format!("run.apply: {e}")),
    };
    // Eligibility gate — refuse (and record `blocked`) for any non-done /
    // unaccepted / inherit run.
    if let Err(reason) = crate::nodes::coordinator::heartbeat::run_apply_eligibility(&run) {
        let _ = store.set_run_apply_status(run_id, "blocked", &reason, 0, 0);
        let _ = store.append_run_event(
            run_id,
            "apply.conflicted",
            "relix",
            &format!("apply refused: {reason}"),
            None,
            false,
        );
        return Err(format!("apply refused: {reason}"));
    }
    let artifacts = match store.list_run_artifacts(run_id) {
        Ok(a) => a,
        Err(e) => return Err(format!("run.apply: {e}")),
    };
    let root = store.run_workspace_config().project_root.clone();
    // Zero-artifact run: a clear no-op (nothing to do). An echo Shift writes
    // nothing, so this is exactly where the safe-local loop closes — advance
    // the Brief to `done` so its dependents unblock.
    if artifacts.is_empty() {
        let _ =
            store.set_run_apply_status(run_id, "applied", "no artifacts — nothing to apply", 0, 0);
        let _ = store.append_run_event(
            run_id,
            "apply.applied",
            "relix",
            "no artifacts — nothing to apply",
            None,
            false,
        );
        let brief_status = advance_reviewed_brief(store, &run.brief_id, run_id);
        return Ok(serde_json::json!({
            "run_id": run_id, "apply_status": "applied",
            "applied_files": 0, "failed_files": 0,
            "brief_id": run.brief_id, "brief_status": brief_status,
        }));
    }
    let outcome = match crate::nodes::coordinator::heartbeat::apply_run(&root, &artifacts) {
        Ok(o) => o,
        Err(e) => {
            let _ = store.set_run_apply_status(
                run_id,
                "failed",
                &format!("invalid project root: {e}"),
                0,
                0,
            );
            let _ = store.append_run_event(
                run_id,
                "apply.failed",
                "relix",
                &format!("apply failed: {e}"),
                None,
                false,
            );
            return Err(format!("run.apply: {e}"));
        }
    };
    // Plan event (always recorded — the preview the apply acted on).
    let _ = store.append_run_event(
        run_id,
        "apply.plan",
        "relix",
        &outcome.plan.note,
        None,
        false,
    );
    if outcome.status == "conflicted" {
        // Refused the whole apply — nothing written.
        let _ = store.set_run_apply_status(run_id, "conflicted", &outcome.plan.note, 0, 0);
        let _ = store.append_run_event(
            run_id,
            "apply.conflicted",
            "relix",
            &format!("apply refused — {}", outcome.plan.note),
            None,
            false,
        );
    } else {
        let _ = store.append_run_event(
            run_id,
            "apply.started",
            "relix",
            &format!(
                "applying {} change(s) to {}",
                outcome.plan.changes, outcome.plan.project_root
            ),
            None,
            false,
        );
        let summary = format!(
            "{} applied, {} failed",
            outcome.applied_files, outcome.failed_files
        );
        let _ = store.set_run_apply_status(
            run_id,
            outcome.status,
            &summary,
            outcome.applied_files as i64,
            outcome.failed_files as i64,
        );
        if outcome.status == "failed" {
            let detail = if outcome.errors.is_empty() {
                summary.clone()
            } else {
                format!("{summary}: {}", outcome.errors.join("; "))
            };
            let _ = store.append_run_event(
                run_id,
                "apply.failed",
                "relix",
                &format!("apply incomplete — {detail}"),
                None,
                false,
            );
        } else {
            let _ = store.append_run_event(
                run_id,
                "apply.applied",
                "relix",
                &format!("apply complete — {summary}"),
                None,
                false,
            );
        }
    }
    // Productized review-to-done (company-model §12.5B/§12.6): ONLY a clean
    // `applied` advances the Brief. A `conflicted` (nothing written) or
    // `failed` (partial) apply leaves the Brief in review — the operator's
    // work is not integrated, so it is NOT done.
    let brief_status = if outcome.status == "applied" {
        advance_reviewed_brief(store, &run.brief_id, run_id)
    } else {
        None
    };
    Ok(serde_json::json!({
        "run_id": run_id,
        "apply_status": outcome.status,
        "applied_files": outcome.applied_files,
        "failed_files": outcome.failed_files,
        "brief_id": run.brief_id,
        "brief_status": brief_status,
        "plan": outcome.plan,
    }))
}

/// Post-startup wiring the per-node-type registration handed
/// back to `run()` because it depends on the `rpc::Client` that
/// only exists after the dispatch bridge is built.
pub(crate) enum StartupWiring {
    /// AI node memory-injection wiring. `cell` was already passed
    /// into `ai::register`; the run() loop populates it post-
    /// startup by building a [`MemoryDispatcher`] from `cfg`.
    AiMemory {
        cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::ai::MemoryFetcher>>>,
        cfg: Option<crate::nodes::ai::AiMemoryPeerConfig>,
    },
    /// Memory node curator wiring. The dispatcher cells were
    /// already passed into `memory::register` and the curator
    /// scheduler; the run() loop populates them post-startup
    /// by building an [`AiMeshDispatcher`] from `cfg.ai_peer`
    /// and a [`CoordMeshDispatcher`] from `cfg.coord_peer`
    /// (each when set). Optionally also carries an embedding
    /// dispatcher cell + config — operators can enable
    /// `[memory.embedding_peer]` independent of the curator.
    MemoryCurator {
        ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::AiDispatcher>>>,
        coord_cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::CoordDispatcher>>>,
        state: Arc<tokio::sync::Mutex<crate::nodes::memory::CuratorState>>,
        cfg: crate::nodes::memory::CuratorConfig,
        embedding_cell:
            Option<Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::EmbeddingDispatcher>>>>,
        embedding_cfg: Option<crate::nodes::memory::EmbeddingPeerConfig>,
    },
    /// Memory node with embedding-only wiring (no curator).
    MemoryEmbedding {
        cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::EmbeddingDispatcher>>>,
        cfg: crate::nodes::memory::EmbeddingPeerConfig,
    },
    /// Coordinator drift-embedder wiring (W4). The cell was
    /// already passed into `coordinator::register`; the run()
    /// loop builds a `MeshDriftEmbedDispatcher` post-startup
    /// by dialing the operator-configured AI peer and
    /// publishes the result into the cell.
    CoordDriftEmbed {
        cell: crate::nodes::ai::guardrails::DriftEmbedDispatcherCell,
        cfg: crate::nodes::coordinator::CoordinatorAiPeerConfig,
    },
    /// Telegram-channel outbound wiring. `cell` was already
    /// passed into the long-poll loop; the run() loop dials
    /// memory + ai + coord peers post-startup and publishes
    /// a [`crate::nodes::telegram::TelegramOutboundClient`]
    /// into it.
    Telegram {
        cell: crate::nodes::telegram::TelegramOutboundClientCell,
        cfg: crate::nodes::telegram::TelegramNodeConfig,
    },
    /// Discord-channel outbound wiring. Same shape as Telegram —
    /// the polling loop already runs; the run() loop dials peers
    /// and publishes the outbound client into `cell`.
    Discord {
        cell: crate::nodes::discord::DiscordOutboundClientCell,
        cfg: crate::nodes::discord::DiscordNodeConfig,
    },
    /// Slack-channel outbound wiring. Same shape as Discord.
    Slack {
        cell: crate::nodes::slack::SlackOutboundClientCell,
        cfg: crate::nodes::slack::SlackNodeConfig,
    },
    /// Email-channel outbound wiring. Same shape as Slack —
    /// the IMAP listener already runs; the run() loop dials
    /// memory + ai + coord peers and publishes the
    /// `EmailOutboundClient` into `cell` so the controller
    /// can reach memory + ai during chat-flow runs.
    ///
    /// `cfg` is boxed because `EmailNodeConfig` is larger than
    /// every other channel config (SMTP + IMAP + DKIM + OAuth2
    /// fields) and would otherwise force the entire enum's stack
    /// footprint to ~1KB.
    Email {
        cell: crate::nodes::email::EmailOutboundClientCell,
        cfg: Box<crate::nodes::email::EmailNodeConfig>,
    },
    /// Coordinator workflow dispatcher wiring (RELIX-7.5).
    /// The cell was already passed into the workflow
    /// capability handlers; the run() loop dials every
    /// configured peer and publishes a
    /// `MeshWorkflowDispatcher` into the cell so
    /// `workflow.run` can dispatch agent steps over the mesh.
    CoordWorkflowDispatcher {
        cell: crate::workflow::WorkflowDispatcherCell,
        peers: std::collections::BTreeMap<String, PeerConfig>,
        deadline_secs: i64,
    },
    /// RELIX-7.11 GAP 3: coordinator-side mesh client for the
    /// `MultiChannelAlertSink`. Dials every peer configured in
    /// `[peers]` and publishes an `AlertMeshContext` into the
    /// cell so alert events can fan out to channel `*.send`
    /// capabilities.
    CoordAlertMesh {
        cell: crate::metrics::AlertMeshCell,
        peers: std::collections::BTreeMap<String, PeerConfig>,
        deadline_secs: i64,
    },
    /// RELIX-7.16 GAP 3: knowledge-mesh dispatcher wiring. The
    /// `KnowledgeService` was constructed with a
    /// [`crate::knowledge::LateBoundDispatcher`] wrapping
    /// `cell`; this wiring builds one
    /// [`crate::knowledge::MeshKnowledgeDispatcher`] per
    /// configured peer (peer alias === node name) and publishes
    /// a [`crate::knowledge::MeshKnowledgeRouter`] into the
    /// cell. Until populated, cross-node shares reject with
    /// `Unreachable { detail: "dispatcher not yet wired" }`.
    KnowledgeMesh {
        cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::knowledge::RemoteKnowledgeDispatcher>>>,
        peers: std::collections::BTreeMap<String, PeerConfig>,
        deadline_secs: i64,
        /// SEC §17: registry to auto-populate with handshake-verified
        /// peer identity keys as connections come up.
        source_key_registry: crate::knowledge::service::SourceNodeKeyRegistry,
    },
}

// Parse `[guardrails]` from the top-level TOML section into
// an `InputGuardrail` instance.
//
// Precedence:
//   1. `[guardrails.input]` block — fine-grained override.
//   2. `[guardrails] mode = "strict"|"balanced"|"permissive"`
//      — mode-driven defaults.
//   3. Neither ⇒ permissive (pre-guardrail behaviour).
fn build_input_guardrail(cfg: &ControllerConfig) -> crate::nodes::ai::guardrails::InputGuardrail {
    use crate::nodes::ai::guardrails::{
        GuardrailMode, InputGuardrail, input::InputGuardrailConfig,
    };
    let Some(raw) = cfg.guardrails.clone() else {
        return InputGuardrail::permissive();
    };
    #[derive(serde::Deserialize, Default)]
    struct GuardrailsBlock {
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        input: Option<InputGuardrailConfig>,
    }
    let parsed: GuardrailsBlock = match raw.try_into() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "[guardrails] parse failed; defaulting to permissive");
            return InputGuardrail::permissive();
        }
    };
    if let Some(ic) = parsed.input {
        return InputGuardrail::from_config(&ic);
    }
    let Some(mode_str) = parsed.mode else {
        return InputGuardrail::permissive();
    };
    match GuardrailMode::parse(&mode_str) {
        Some(m) => InputGuardrail::from_mode(m),
        None => {
            tracing::warn!(
                mode = %mode_str,
                "[guardrails] unknown mode; defaulting to permissive"
            );
            InputGuardrail::permissive()
        }
    }
}

/// W7: build the runtime `OtelConfig` from the controller's
/// `[observability.otel]` block. Returns `None` when no block
/// is configured. The returned config carries `enabled: true`
/// only when both the section's `enabled` flag is set AND an
/// endpoint URL is provided — otherwise the exporter stays
/// dormant so a misconfigured operator doesn't sprout a stray
/// outbound dependency.
pub(crate) fn build_otel_config(
    cfg: &ControllerConfig,
) -> Option<crate::observability::OtelConfig> {
    let obs = cfg.observability.as_ref()?;
    let otel = obs.otel.as_ref()?;
    if !otel.enabled || otel.endpoint.is_none() {
        return None;
    }
    let mut runtime_cfg = crate::observability::OtelConfig {
        enabled: true,
        endpoint_url: otel.endpoint.clone(),
        ..crate::observability::OtelConfig::default()
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
    Some(runtime_cfg)
}

/// RELIX-7.11 — boot-time bundle of every piece the metrics
/// subsystem needs. Built once in `run()` so the bridge wiring,
/// the coordinator capability registration, and the alert
/// engine all see the same store + query handle + price table.
pub(crate) struct MetricsBundle {
    pub sink: std::sync::Arc<dyn crate::metrics::MetricsSink>,
    /// RELIX-7.28 Part 1: cheap-clone handle on the concrete
    /// collector behind `sink`. Used by the controller wiring
    /// path to call `set_budget_enforcer` without resorting to
    /// trait-object downcasts.
    pub collector: crate::metrics::MetricsCollector,
    pub query: crate::metrics::MetricsQuery,
    pub alert_engine: crate::metrics::AlertEngine,
    pub alert_interval_secs: u64,
    /// RELIX-7.11 GAP 3/4: chronicle for alert events + the
    /// configured channel fan-out targets + the OnceCell the
    /// post-startup wiring populates with an `AlertMeshContext`
    /// once the mesh client is up.
    pub alert_chronicle: crate::metrics::AlertChronicle,
    pub alert_targets: Vec<crate::metrics::AlertTarget>,
    pub alert_mesh_cell: crate::metrics::AlertMeshCell,
    /// GAP 22 Feature 2 follow-up: parsed `[metrics.cost_alerts]`.
    /// Carried in the bundle so the coordinator branch reads it
    /// without re-parsing the controller TOML.
    pub cost_alerts_cfg: crate::metrics::spike_detector::CostAlertsConfig,
    /// GAP 22 Feature 2 follow-up: resolved metrics SQLite path —
    /// used by the spike detector to derive a sibling
    /// `cost_baselines.db` when the operator hasn't set an
    /// explicit override.
    pub metrics_db_path: std::path::PathBuf,
}

/// RELIX-7.19 GAP 4 — boot-time bundle of every piece the
/// confidence subsystem needs. Built once in `run()` so the
/// dispatch bridge, the `confidence.*` coordinator caps, and
/// the SOL `last_confidence()` cell all share the same
/// scorer + engine + cell. Returned by
/// [`build_confidence_bundle`]; consumed by both the bridge
/// wiring AND the SOL flow_runner (web bridge constructs
/// `FlowRunOptions.last_confidence_cell` from a cheap clone
/// of `bundle.cell`).
pub(crate) struct ConfidenceBundle {
    pub scorer: std::sync::Arc<crate::confidence::ConfidenceScorer>,
    pub engine: std::sync::Arc<crate::confidence::FallbackEngine>,
    pub cell: crate::confidence::LastConfidenceCell,
    /// RELIX-7.29 PART 2: process-wide self-consistency
    /// counters surfaced by `confidence.self_consistency_stats`.
    pub sc_stats: crate::confidence::SelfConsistencyStats,
    /// RELIX-7.29 PART 2: the configured SC block (or its
    /// default when none was set) — surfaced verbatim by the
    /// stats cap so operators can confirm what's live.
    pub sc_cfg: crate::confidence::SelfConsistencyConfig,
}

/// RELIX-7.19 GAP 4: construct the confidence bundle from
/// `[confidence]`. Returns `Ok(None)` when the section is
/// absent OR `enabled = false` — the dispatch bridge then
/// stays in pre-7.19 byte-identical mode.
pub(crate) fn build_confidence_bundle(
    cfg: &ControllerConfig,
) -> Result<Option<ConfidenceBundle>, Box<dyn std::error::Error>> {
    build_confidence_bundle_from(cfg.confidence.as_ref())
}

/// RELIX-7.19 GAP 4: internal builder pulled out of
/// `build_confidence_bundle` so unit tests can exercise the
/// branching without standing up a full `ControllerConfig`
/// (which carries 20+ required sections).
pub(crate) fn build_confidence_bundle_from(
    cfg: Option<&crate::confidence::ConfidenceConfig>,
) -> Result<Option<ConfidenceBundle>, Box<dyn std::error::Error>> {
    let c_cfg = match cfg {
        Some(c) if c.enabled => c.clone(),
        _ => return Ok(None),
    };
    let scorer = std::sync::Arc::new(crate::confidence::ConfidenceScorer::from_config(&c_cfg));
    let engine = std::sync::Arc::new(crate::confidence::FallbackEngine::from_policies(
        &c_cfg.policies,
    ));
    let cell = crate::confidence::LastConfidenceCell::new();
    tracing::info!(
        window_size = c_cfg.window_size,
        policy_count = c_cfg.policies.len(),
        p95_baseline_ms = c_cfg.p95_latency_baseline_ms,
        "confidence: scorer + fallback engine online (RELIX-7.19)"
    );
    Ok(Some(ConfidenceBundle {
        scorer,
        engine,
        cell,
        sc_stats: crate::confidence::SelfConsistencyStats::new(),
        sc_cfg: c_cfg.self_consistency.clone().unwrap_or_default(),
    }))
}

/// RELIX-7.15: training-data pipeline bundle. Returned by
/// [`build_training_bundle`] and consumed by both the AI node
/// (to attach the recorder sink) and the coordinator node (to
/// register the six `training.*` capabilities).
pub(crate) struct TrainingBundle {
    pub sink: std::sync::Arc<dyn crate::training::InteractionSink>,
    pub store: crate::training::TrainingStore,
    pub export_dir: std::path::PathBuf,
    /// RELIX-7.15 PII anonymizer applied by the recorder at
    /// record time AND by the export engine as a safety net
    /// for any row that was recorded before
    /// `[training.pii] enabled = true` got flipped on.
    pub anonymizer: std::sync::Arc<crate::training::PiiAnonymizer>,
}

/// Open the training store, spawn the drain + retention +
/// scorer loops, and return the bundle the AI handler +
/// coordinator both need.
///
/// Returns `Ok(None)` when `[training] enabled = false` (or
/// the section is absent) — the AI handler then runs without
/// an interaction sink and the `training.*` capabilities stay
/// unregistered.
pub(crate) fn build_training_bundle(
    cfg: &ControllerConfig,
    data_dir: &std::path::Path,
) -> Result<Option<TrainingBundle>, Box<dyn std::error::Error>> {
    let t_cfg = match cfg.training.clone() {
        Some(c) if c.enabled => c,
        _ => return Ok(None),
    };
    let db_path = t_cfg
        .db_path
        .clone()
        .unwrap_or_else(|| crate::training::default_training_path(data_dir));
    let store = crate::training::TrainingStore::open(&db_path)
        .map_err(|e| format!("[training] open {}: {e}", db_path.display()))?;
    // RELIX-7.15 PII: build the anonymizer + per-agent
    // policies before constructing the recorder so both the
    // record-time and the export-time paths share the same
    // resolved instances. Operators opt in via
    // `[training.pii] enabled = true`; the default
    // configuration leaves anonymization OFF for backwards
    // compatibility with pre-PII deployments.
    let anonymizer = std::sync::Arc::new(crate::training::PiiAnonymizer::from_config(&t_cfg.pii));
    let agent_policies = std::sync::Arc::new(build_agent_training_policies(cfg, &t_cfg.pii));
    let (recorder, handles) = crate::training::InteractionRecorder::new_with(
        store.clone(),
        anonymizer.clone(),
        agent_policies,
    );
    let _spawned = handles.spawn(crate::training::RetentionConfig {
        retention_days: t_cfg.retention_days,
        sweep_interval: std::time::Duration::from_secs(t_cfg.retention_sweep_interval_secs.max(60)),
    });
    if t_cfg.scorer_enabled {
        let _scorer = crate::training::spawn_scorer_loop(
            store.clone(),
            crate::training::ScorerConfig {
                interval: std::time::Duration::from_secs(t_cfg.scorer_interval_secs.max(5)),
                batch_size: t_cfg.scorer_batch_size,
            },
        );
        tracing::info!(
            interval_secs = t_cfg.scorer_interval_secs,
            batch_size = t_cfg.scorer_batch_size,
            "training: background quality scorer spawned"
        );
    }
    let export_dir = t_cfg
        .export_dir
        .clone()
        .unwrap_or_else(|| crate::training::default_export_dir(data_dir));
    tracing::info!(
        db = %db_path.display(),
        retention_days = t_cfg.retention_days,
        scorer_enabled = t_cfg.scorer_enabled,
        export_dir = %export_dir.display(),
        pii_enabled = t_cfg.pii.enabled,
        pii_strategy = %t_cfg.pii.strategy.as_str(),
        "training: recorder + retention online"
    );
    Ok(Some(TrainingBundle {
        sink: std::sync::Arc::new(recorder),
        store,
        export_dir,
        anonymizer,
    }))
}

/// Build the per-agent training policies map from the
/// top-level `[agents.<name>.training]` config blocks. Agents
/// without a block inherit the global behaviour (training
/// enabled, global PII anonymizer). The global `[training.pii]`
/// config is passed in so per-agent overrides can re-use the
/// global type-overrides table as a baseline.
fn build_agent_training_policies(
    cfg: &ControllerConfig,
    pii: &crate::training::PiiConfig,
) -> crate::training::AgentTrainingPolicies {
    let mut policies = crate::training::AgentTrainingPolicies::empty();
    for (name, agent_cfg) in &cfg.agents {
        let Some(training_cfg) = agent_cfg.training.as_ref() else {
            continue;
        };
        if let Some(enabled) = training_cfg.enabled {
            policies.enabled.insert(name.clone(), enabled);
        }
        if let Some(strategy_str) = training_cfg.pii_strategy.as_ref() {
            let Some(strategy) = crate::training::PiiStrategy::parse(strategy_str) else {
                tracing::warn!(
                    agent = %name,
                    strategy = %strategy_str,
                    "[agents.<name>.training.pii_strategy]: unknown strategy; agent inherits global"
                );
                continue;
            };
            // Build a per-agent PiiConfig that inherits the
            // global `overrides` table but uses the per-agent
            // strategy as the default. PII is always considered
            // ENABLED when the operator wrote a per-agent
            // strategy — otherwise the override has no effect.
            let per_agent = crate::training::PiiConfig {
                enabled: true,
                strategy,
                overrides: pii.overrides.clone(),
            };
            policies.anonymizers.insert(
                name.clone(),
                std::sync::Arc::new(crate::training::PiiAnonymizer::from_config(&per_agent)),
            );
        }
    }
    policies
}

/// Open the metrics store, spawn the drain + retention loops,
/// and return everything the caller needs to wire the sink onto
/// the dispatch bridge + register coordinator caps.
///
/// Returns `Ok(None)` when `[metrics] enabled = false` (or the
/// section is absent) — the bridge boots without a sink and the
/// dispatch path stays counter-only.
pub(crate) fn build_metrics_bundle(
    cfg: &ControllerConfig,
    data_dir: &std::path::Path,
) -> Result<Option<MetricsBundle>, Box<dyn std::error::Error>> {
    let m_cfg = match cfg.metrics.clone() {
        Some(c) if c.enabled => c,
        _ => return Ok(None),
    };
    let db_path = m_cfg
        .db_path
        .clone()
        .unwrap_or_else(|| crate::metrics::default_metrics_path(data_dir));
    let store = crate::metrics::MetricsStore::open(&db_path)
        .map_err(|e| format!("[metrics] open {}: {e}", db_path.display()))?;
    let prices = m_cfg.prices.clone().into_table();
    let (collector, handles) = crate::metrics::MetricsCollector::new(store.clone(), prices);
    let _spawned = handles.spawn(crate::metrics::RetentionConfig {
        retention_days: m_cfg.retention_days,
        sweep_interval: std::time::Duration::from_secs(m_cfg.retention_sweep_interval_secs.max(60)),
    });
    let query = crate::metrics::MetricsQuery::new(store.clone());
    let alert_engine = crate::metrics::AlertEngine::new(query.clone(), m_cfg.thresholds.clone());

    // RELIX-7.11 GAP 4: alert chronicle. Drops next to the
    // metrics db unless the operator overrides
    // `[metrics.alerts.chronicle_path]`.
    let chronicle_path = m_cfg.alerts.chronicle_path.clone().unwrap_or_else(|| {
        db_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("alerts.sqlite")
    });
    let alert_chronicle = crate::metrics::AlertChronicle::open(&chronicle_path).map_err(|e| {
        format!(
            "[metrics.alerts] chronicle open {}: {e}",
            chronicle_path.display()
        )
    })?;
    tracing::info!(
        db = %db_path.display(),
        chronicle = %chronicle_path.display(),
        retention_days = m_cfg.retention_days,
        alert_interval_secs = m_cfg.alert_interval_secs,
        alert_targets = m_cfg.alerts.targets.len(),
        "metrics: collector + query + alert engine + chronicle online"
    );
    Ok(Some(MetricsBundle {
        sink: std::sync::Arc::new(collector.clone()),
        collector,
        query,
        alert_engine,
        alert_interval_secs: m_cfg.alert_interval_secs,
        alert_chronicle,
        alert_targets: m_cfg.alerts.targets.clone(),
        alert_mesh_cell: std::sync::Arc::new(tokio::sync::OnceCell::new()),
        cost_alerts_cfg: m_cfg.cost_alerts.clone(),
        metrics_db_path: db_path,
    }))
}

/// RELIX-7.28 Part 1 — boot-time bundle for the budget enforcer. The
/// enforcer is cheap to clone (`Arc<BudgetInner>`); we wrap it once so
/// the dispatch bridge + the coordinator's `budget.*` capabilities see
/// the same instance.
pub(crate) struct BudgetBundle {
    pub enforcer: std::sync::Arc<crate::metrics::BudgetEnforcer>,
}

/// Build the budget bundle. Returns `None` when:
///
/// - `[budget]` is absent.
/// - The config has no agent OR deployment cap configured.
///
/// In either case the dispatch path skips the enforcer entirely.
pub(crate) fn build_budget_bundle(
    cfg: &ControllerConfig,
    metrics: Option<&MetricsBundle>,
) -> Option<BudgetBundle> {
    let bcfg = cfg.budget.clone()?;
    if !bcfg.is_active() {
        return None;
    }
    let query = metrics.map(|m| m.query.clone());
    let enforcer = std::sync::Arc::new(crate::metrics::BudgetEnforcer::new(bcfg, query));
    tracing::info!(
        active = enforcer.is_active(),
        throttle_backoff_ms = enforcer.throttle_backoff().as_millis() as u64,
        "budget: enforcer online (RELIX-7.28 Part 1)"
    );
    Some(BudgetBundle { enforcer })
}

/// RELIX-7.28 Part 3 — open the mesh PII gate when `[mesh_pii]` is
/// configured + enabled. Returns `Ok(None)` for the default-off case.
pub(crate) fn build_pii_gate_bundle(
    cfg: &ControllerConfig,
    data_dir: &std::path::Path,
) -> Result<Option<std::sync::Arc<crate::nodes::pii_gate::MeshPiiGate>>, Box<dyn std::error::Error>>
{
    let pcfg = match cfg.mesh_pii.clone() {
        Some(c) => c,
        None => return Ok(None),
    };
    if !pcfg.enabled {
        return Ok(None);
    }
    let path = pcfg
        .chronicle_path
        .clone()
        .unwrap_or_else(|| crate::nodes::pii_gate::default_pii_chronicle_path(data_dir));
    let gate = crate::nodes::pii_gate::MeshPiiGate::from_config(pcfg, &path)
        .map_err(|e| format!("[mesh_pii] open {}: {e}", path.display()))?;
    Ok(gate.map(std::sync::Arc::new))
}

/// Static descriptor list for the six metrics capabilities
/// registered on the coordinator. Used for manifest entry +
/// future dashboard rendering.
pub(crate) fn metrics_capability_descriptors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "metrics.agents",
            "List every agent with metrics in the last N hours, with a per-agent summary. \
             Args: optional JSON { hours }; default 24.",
        ),
        (
            "metrics.agent_summary",
            "Per-agent summary (invocations / success rate / P50/P95/P99 latency / total tokens / \
             total cost / common error) over the last N hours. Args: JSON { agent, hours? }.",
        ),
        (
            "metrics.method_breakdown",
            "Per-method breakdown (same fields as agent_summary, grouped by method) for an agent. \
             Args: JSON { agent, method?, hours? }.",
        ),
        (
            "metrics.timeseries",
            "Bucketed time-series for an agent. Args: JSON { agent, hours?, bucket_minutes? }; \
             default hours=24, bucket_minutes=5.",
        ),
        (
            "metrics.alerts_active",
            "Snapshot of currently-active alerts with severity, triggered_at, agent, kind, \
             threshold, and actual value.",
        ),
        (
            "metrics.cost_report",
            "Cost breakdown by (agent, method) over the last N hours. Sorted by total cost \
             descending. Args: optional JSON { hours }.",
        ),
    ]
}

/// W2: build an `AgentAccessBroker` from the controller's
/// `[[execution.agents]]` config. Absent / empty config
/// produces an empty broker — `check()` returns Allow for
/// every (agent, capability) pair so existing deployments
/// without policies behave identically.
pub(crate) fn build_access_broker(
    cfg: &ControllerConfig,
) -> std::sync::Arc<crate::nodes::execution::broker::AgentAccessBroker> {
    use crate::nodes::execution::broker::AgentAccessBroker;
    match cfg.execution.as_ref() {
        Some(exec) if !exec.agents.is_empty() => {
            tracing::info!(
                agents = exec.agents.len(),
                "execution: loaded [[execution.agents]] policies into AgentAccessBroker"
            );
            std::sync::Arc::new(AgentAccessBroker::new(exec.agents.clone()))
        }
        _ => std::sync::Arc::new(AgentAccessBroker::empty()),
    }
}

// Parse `[guardrails.drift]` from the top-level [guardrails]
// section. Returns `Some(cfg)` only when `enabled = true`; an
// absent / disabled config returns `None` so the coordinator's
// drift hook short-circuits immediately.

/// GAP 4 — bundle of the SkillStore + extractor + refinement
/// engine produced by [`build_skills_runtime`]. The same store
/// is shared by every consumer: the AI handler holds the
/// extractor (for the post-`ai.chat` hook), the coordinator
/// bridge holds the store (for the six skill caps), and the
/// refinement engine background task gets spawned from the
/// same Arc so all three see the same SQLite rows.
pub(crate) struct SkillsRuntime {
    pub store: std::sync::Arc<crate::nodes::ai::skill_store::SkillStore>,
    pub extractor: Option<std::sync::Arc<crate::nodes::ai::skill_extractor::SkillExtractor>>,
    pub refinement:
        Option<std::sync::Arc<crate::nodes::ai::skill_refinement::SkillRefinementEngine>>,
}

/// GAP 4: parse `[skills]` and, when enabled, construct the
/// shared [`SkillsRuntime`] bundle. Returns `None` for the
/// config-absent / disabled / unparseable cases.
fn build_skills_runtime(
    raw: &Option<toml::Value>,
    provider: std::sync::Arc<dyn crate::nodes::ai::provider::ChatProvider>,
    default_model: &str,
) -> Option<SkillsRuntime> {
    use crate::nodes::ai::skill_extractor::{
        LocalProviderAiDispatcher, LocalProviderEmbedDispatcher, SkillExtractor,
        SkillExtractorConfig,
    };
    use crate::nodes::ai::skill_refinement::{RefinementConfig, SkillRefinementEngine};
    use crate::nodes::ai::skill_store::SkillStore;
    use crate::nodes::ai::skills::SkillsConfig;
    use crate::nodes::memory::curator::{AiDispatcher, EmbeddingDispatcher};
    let raw = raw.clone()?;
    let cfg: SkillsConfig = match raw.try_into() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "[skills] parse failed; skill runtime disabled");
            return None;
        }
    };
    if !cfg.enabled {
        return None;
    }
    let db_path = match cfg.db_path.as_ref() {
        Some(p) => p.clone(),
        None => {
            tracing::warn!("[skills] enabled = true but db_path missing; skill runtime disabled");
            return None;
        }
    };
    let store = match SkillStore::open(&db_path) {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => {
            tracing::warn!(error = %e, path = %db_path.display(), "[skills] open store failed");
            return None;
        }
    };
    let extraction_model = cfg
        .extraction_model
        .clone()
        .unwrap_or_else(|| default_model.to_string());
    let ai_dispatcher: std::sync::Arc<dyn AiDispatcher> = std::sync::Arc::new(
        LocalProviderAiDispatcher::new(provider.clone(), extraction_model.clone()),
    );
    let embed_dispatcher: std::sync::Arc<dyn EmbeddingDispatcher> =
        std::sync::Arc::new(LocalProviderEmbedDispatcher::new(provider.clone()));
    let ai_cell: std::sync::Arc<tokio::sync::OnceCell<std::sync::Arc<dyn AiDispatcher>>> =
        std::sync::Arc::new(tokio::sync::OnceCell::new());
    let _ = ai_cell.set(ai_dispatcher);
    let embed_cell: std::sync::Arc<tokio::sync::OnceCell<std::sync::Arc<dyn EmbeddingDispatcher>>> =
        std::sync::Arc::new(tokio::sync::OnceCell::new());
    let _ = embed_cell.set(embed_dispatcher);
    let default_embedding = SkillExtractorConfig::default().embedding_model;
    let ex_cfg = SkillExtractorConfig {
        extraction_model: extraction_model.clone(),
        min_complexity_score: cfg.min_complexity_score,
        dup_threshold: cfg.dup_threshold,
        embedding_model: cfg.embedding_model.clone().unwrap_or(default_embedding),
        ..SkillExtractorConfig::default()
    };
    let extractor = if cfg.auto_extract {
        Some(std::sync::Arc::new(SkillExtractor::new(
            store.clone(),
            ai_cell.clone(),
            embed_cell,
            ex_cfg,
        )))
    } else {
        None
    };
    let refinement = if cfg.refinement_enabled {
        let rcfg = RefinementConfig {
            model: extraction_model,
            ..RefinementConfig::default()
        };
        Some(std::sync::Arc::new(SkillRefinementEngine::new(
            store.clone(),
            ai_cell,
            rcfg,
        )))
    } else {
        None
    };
    tracing::info!(
        path = %db_path.display(),
        auto_extract = cfg.auto_extract,
        refinement_enabled = cfg.refinement_enabled,
        "skill runtime: enabled"
    );
    Some(SkillsRuntime {
        store,
        extractor,
        refinement,
    })
}

/// GAP 11: compensating-call dispatcher used by
/// `execution.rollback`. Wraps the local `ToolDispatcher` so
/// Tier A rollbacks ride the same admission pipeline (broker
/// check, secret resolution, evidence capture) as the original
/// dispatch.
struct LocalCompensatingDispatcher {
    inner: std::sync::Arc<crate::nodes::tool::dispatcher::ToolDispatcher>,
}

impl LocalCompensatingDispatcher {
    fn new(inner: std::sync::Arc<crate::nodes::tool::dispatcher::ToolDispatcher>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl crate::nodes::execution::rollback::CompensatingDispatcher for LocalCompensatingDispatcher {
    async fn invoke(&self, tool: &str, args_json: &str) -> Result<String, String> {
        use crate::nodes::execution::gateway_tier::GatewayDispatchOptions;
        // Tier A compensation actions themselves run as Tier B
        // — a successful rollback is irreversible; the operator
        // accepts that.
        let opts = GatewayDispatchOptions::default()
            .human_rollback_plan(format!("auto-compensation for {tool}"));
        // The compensating call has no local handler — we route
        // it through whichever capability the original tool
        // author declared. The closure here is a passthrough;
        // the bridge's outbound dispatcher will be reached via
        // the manifest's routing.
        //
        // Honest scope: in the alpha the compensating call must
        // be a tool the same controller owns OR a tool reached
        // through the existing tool_mesh dispatcher. When the
        // tool isn't local, the closure returns an error and
        // the rollback row carries `success: false` so the
        // operator sees it.
        let arg = args_json.to_string();
        let tool_owned = tool.to_string();
        let tool_for_closure = tool_owned.clone();
        self.inner
            .dispatch_with_options("rollback", &tool_owned, &arg, opts, move |a| async move {
                // Surface "not implemented locally" rather than
                // attempting to fan out to a mesh peer; the
                // gateway can't safely re-enter the dispatch
                // bridge from inside a handler without a full
                // tool-mesh wiring (which lives on the
                // controller binary, not the runtime crate).
                Err(format!(
                    "compensating call to `{tool_for_closure}` not handled locally (args={a})"
                ))
            })
            .await
            .map_err(|e| e.to_string())
    }
}

/// GAP 13 + 14: build the AI controller's own two-sink
/// `ObservabilityContext`. Returns `None` when the operator
/// did not enable `[observability.two_sink]`. On any open
/// error, logs a warn and returns `None` — the AI handler then
/// short-circuits its provenance + metadata hooks.
fn build_ai_observability(
    cfg: &ControllerConfig,
) -> Option<std::sync::Arc<crate::observability::ObservabilityContext>> {
    use crate::observability::{
        ContentSink, MetadataSink, ObservabilityContext, ProvenanceRegistry,
    };
    let obs = cfg.observability.as_ref()?;
    let ts = obs.two_sink.as_ref()?;
    if !ts.enabled {
        return None;
    }
    let metadata_path = ts.metadata_db_path.clone()?;
    let metadata = match MetadataSink::open(&metadata_path) {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => {
            tracing::warn!(error = %e, path = %metadata_path.display(), "[observability.two_sink] open metadata sink failed");
            return None;
        }
    };
    let content_path = ts.content_db_path.clone().unwrap_or_else(|| {
        let mut p = metadata_path.clone();
        p.set_file_name("content.db");
        p
    });
    let content = match ContentSink::open(&content_path, ts.content_retention_days) {
        Ok(s) => std::sync::Arc::new(s),
        Err(e) => {
            tracing::warn!(error = %e, path = %content_path.display(), "[observability.two_sink] open content sink failed");
            return None;
        }
    };
    let provenance_path = ts.provenance_db_path.clone().unwrap_or_else(|| {
        let mut p = metadata_path.clone();
        p.set_file_name("provenance.db");
        p
    });
    let provenance = match ProvenanceRegistry::open(&provenance_path) {
        Ok(r) => std::sync::Arc::new(r),
        Err(e) => {
            tracing::warn!(error = %e, path = %provenance_path.display(), "[observability.two_sink] open provenance registry failed");
            return None;
        }
    };
    tracing::info!(
        metadata_db = %metadata_path.display(),
        content_db = %content_path.display(),
        provenance_db = %provenance_path.display(),
        "ai observability: two-sink context wired",
    );
    Some(std::sync::Arc::new(ObservabilityContext::new(
        metadata, content, provenance,
    )))
}

fn build_drift_config(cfg: &ControllerConfig) -> Option<crate::nodes::ai::guardrails::DriftConfig> {
    use crate::nodes::ai::guardrails::DriftConfig;
    let raw = cfg.guardrails.clone()?;
    #[derive(serde::Deserialize, Default)]
    struct GuardrailsBlock {
        #[serde(default)]
        drift: Option<DriftConfig>,
    }
    let parsed: GuardrailsBlock = raw
        .try_into()
        .map_err(|e: toml::de::Error| {
            tracing::warn!(error = %e, "[guardrails] parse failed; drift hook disabled");
        })
        .ok()?;
    parsed.drift.filter(|c| c.enabled)
}

/// RELIX-7.29 follow-up — open a layered store handle for the
/// belief tracker's cross-restart persistence. Wired ONLY when
/// `[ai.belief_state] enabled = true` AND the AI controller's
/// process also has `[memory]` configured with a layered store
/// path (typical for combined AI+memory single-process
/// deployments). Multi-process deployments leave this `None`
/// and the tracker stays process-local.
///
/// Note: this opens a SECOND in-process handle to the same
/// SQLite file the memory node uses. SQLite handles multiple
/// handles via WAL mode (the default Relix uses); both handles
/// see the same rows and writes from one are visible to the
/// other after commit.
fn build_belief_persistence_store(
    cfg: &ControllerConfig,
    ai_cfg: &crate::nodes::ai::AiConfig,
) -> Option<std::sync::Arc<crate::nodes::memory::schema::LayeredMemoryStore>> {
    use crate::nodes::memory::schema::LayeredMemoryStore;
    let belief_enabled = ai_cfg
        .belief_state
        .as_ref()
        .map(|b| b.enabled)
        .unwrap_or(false);
    if !belief_enabled {
        return None;
    }
    let raw_mem = cfg.memory.as_ref()?;
    let mem_cfg: crate::nodes::memory::MemoryConfig = match raw_mem.clone().try_into() {
        Ok(c) => c,
        Err(_) => return None,
    };
    let want_qdrant = mem_cfg
        .qdrant
        .as_ref()
        .is_some_and(|q| !q.url.trim().is_empty());
    if !want_qdrant && mem_cfg.layered_db_path.is_none() {
        return None;
    }
    let path = mem_cfg.layered_db_path.clone().unwrap_or_else(|| {
        // Same sidecar derivation as `open_layered_memory` so
        // the belief tracker writes to the same file the
        // memory node owns.
        let mut p = mem_cfg.db_path.clone();
        let stem = p.file_stem().map(|s| s.to_owned()).unwrap_or_default();
        let new_name = format!("{}.layered.db", stem.to_string_lossy());
        p.set_file_name(new_name);
        p
    });
    match LayeredMemoryStore::open(&path) {
        Ok(store) => {
            tracing::info!(
                path = %path.display(),
                "belief tracker: layered store handle wired for cross-restart persistence"
            );
            Some(std::sync::Arc::new(store))
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "belief tracker: failed to open layered store; persistence disabled"
            );
            None
        }
    }
}

// Open the four-layer LayeredMemoryStore when the operator opted
// in via [memory.qdrant] OR an explicit layered_db_path. Returns
// None when neither is configured — the layered surface is purely
// additive infrastructure and absent config means it stays off.
fn open_layered_memory(
    mem_cfg: &crate::nodes::memory::MemoryConfig,
) -> Result<Option<crate::nodes::memory::LayeredContext>, Box<dyn std::error::Error>> {
    use crate::nodes::memory::schema::LayeredMemoryStore;
    let want_qdrant = mem_cfg
        .qdrant
        .as_ref()
        .is_some_and(|q| !q.url.trim().is_empty());
    if !want_qdrant && mem_cfg.layered_db_path.is_none() {
        return Ok(None);
    }
    let path = mem_cfg.layered_db_path.clone().unwrap_or_else(|| {
        // Sidecar DB next to the primary memory.db: a
        // `mem.db` becomes `mem.layered.db`. Keeps the two
        // SQLite files on the same filesystem (so they
        // share the same backup story) without colliding
        // on the file name.
        let mut p = mem_cfg.db_path.clone();
        let stem = p.file_stem().map(|s| s.to_owned()).unwrap_or_default();
        let new_name = format!("{}.layered.db", stem.to_string_lossy());
        p.set_file_name(new_name);
        p
    });
    let store = std::sync::Arc::new(
        LayeredMemoryStore::open(&path).map_err(|e| format!("[memory] layered store open: {e}"))?,
    );
    let qdrant = mem_cfg
        .qdrant
        .as_ref()
        .filter(|q| !q.url.trim().is_empty())
        .map(|qcfg| {
            std::sync::Arc::new(crate::nodes::memory::qdrant::QdrantClient::new(
                qcfg.clone(),
            ))
        });
    let score_threshold = mem_cfg
        .embedder
        .as_ref()
        .map(|e| e.score_threshold)
        .unwrap_or(0.75);
    // RELIX-7.15 memory PII: build the anonymizer once and
    // share it with the layered surface. The recorder at
    // memory.write_turn AND the defensive promoter / embedder
    // passes all read from this same instance, so operators
    // get consistent redaction across every layer.
    let anonymizer = std::sync::Arc::new(crate::training::PiiAnonymizer::from_config(&mem_cfg.pii));
    Ok(Some(crate::nodes::memory::LayeredContext {
        store,
        qdrant,
        score_threshold,
        anonymizer,
    }))
}

/// SEC §13: the `node_type` values the controller actually
/// implements on the non-router path. Each registers a real
/// capability surface in [`register_node_type_handlers`]. A
/// `node_type` outside this set registers ZERO node-type
/// capabilities — booting it yields a process that logs
/// "controller online" while doing nothing. (Router nodes take a
/// separate `role = "router"` path and never reach that function;
/// the web bridge is the separate `relix-web-bridge` binary.)
pub const SUPPORTED_CONTROLLER_NODE_TYPES: &[&str] = &[
    "memory",
    "ai",
    "coordinator",
    "telegram",
    "discord",
    "slack",
    "email",
    "plugin_host",
    "tool",
];

/// SEC §13: fail closed on an unhandled / no-op `node_type`.
/// Previously `node_type = "web_bridge"` / `"demo"` (and any typo)
/// fell through every branch and the controller booted "online"
/// with zero capabilities — a dead process that looks healthy.
/// Now such a `node_type` is a hard error with a clear message.
fn validate_controller_node_type(node_type: &str) -> Result<(), String> {
    if SUPPORTED_CONTROLLER_NODE_TYPES.contains(&node_type) {
        return Ok(());
    }
    Err(format!(
        "node_type=`{node_type}` is not implemented by the controller — it would register zero \
         capabilities and boot a dead process that merely logs \"controller online\". Supported \
         controller node types: {}. (A web bridge runs as the separate `relix-web-bridge` binary; \
         a router node sets `role = \"router\"`.)",
        SUPPORTED_CONTROLLER_NODE_TYPES.join(", ")
    ))
}

/// Register node-type-specific capabilities based on `[controller] node_type`.
///
/// SEC §13: a `node_type` outside [`SUPPORTED_CONTROLLER_NODE_TYPES`]
/// is rejected up front (see [`validate_controller_node_type`]) so
/// the controller never boots a no-op process. Each supported type
/// below registers its real capability surface.
#[allow(clippy::too_many_arguments)]
fn register_node_type_handlers(
    bridge: &mut DispatchBridge,
    cfg: &ControllerConfig,
    manifest: ManifestProvider,
    access_broker: std::sync::Arc<crate::nodes::execution::broker::AgentAccessBroker>,
    out: &mut Vec<StartupWiring>,
    metrics: Option<&MetricsBundle>,
    training: Option<&TrainingBundle>,
    budget: Option<&BudgetBundle>,
    pii_gate: Option<&std::sync::Arc<crate::nodes::pii_gate::MeshPiiGate>>,
    confidence_bundle: Option<&ConfidenceBundle>,
) -> Result<(), Box<dyn std::error::Error>> {
    use relix_core::capability::CapabilityDescriptor;

    // SEC §13: refuse to boot an unimplemented / no-op node_type
    // instead of silently coming "online" with zero capabilities.
    validate_controller_node_type(&cfg.controller.node_type)?;

    if cfg.controller.node_type == "memory" {
        let raw = cfg.memory.clone().ok_or_else(|| {
            "node_type=memory requires a [memory] section with db_path".to_string()
        })?;
        let mem_cfg: crate::nodes::memory::MemoryConfig = raw
            .try_into()
            .map_err(|e: toml::de::Error| format!("[memory] parse: {e}"))?;
        let store = std::sync::Arc::new(crate::nodes::memory::MemoryStore::open(&mem_cfg)?);
        // Shared AI dispatcher cell — passed to both the
        // `memory.agent_curate` handler and the curator
        // scheduler so manual + scheduled paths use the same
        // dispatcher once it's populated post-startup.
        let curator_ai_cell: Arc<
            tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::AiDispatcher>>,
        > = Arc::new(tokio::sync::OnceCell::new());
        // Shared coordinator-dispatcher cell — used by the
        // scheduler to write `memory.curator_run` chronicle
        // events. Empty cell == coord_peer not configured, so
        // events are silently skipped (one WARN per tick).
        let curator_coord_cell: Arc<
            tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::CoordDispatcher>>,
        > = Arc::new(tokio::sync::OnceCell::new());
        let curator_state: Arc<tokio::sync::Mutex<crate::nodes::memory::CuratorState>> = Arc::new(
            tokio::sync::Mutex::new(crate::nodes::memory::CuratorState::default()),
        );
        // The new `memory.curator_status` capability reads real
        // CuratorState; pass it as `(state, cfg)` if [memory.
        // curator] is set, else None — handler returns a clear
        // "configured=false" body.
        let curator_handler_cfg = mem_cfg
            .curator
            .clone()
            .map(|c| (curator_state.clone(), Arc::new(c)));
        // Embedding-dispatcher cell — populated post-startup
        // when [memory.embedding_peer] is configured. Empty cell
        // makes memory.embed / memory.search return a clear
        // "not configured" error rather than crashing.
        let embedding_cell: Arc<
            tokio::sync::OnceCell<Arc<dyn crate::nodes::memory::EmbeddingDispatcher>>,
        > = Arc::new(tokio::sync::OnceCell::new());
        let embedding_model = mem_cfg
            .embedding_peer
            .as_ref()
            .map(|p| p.model.clone())
            .unwrap_or_else(|| "text-embedding-3-small".to_string());
        // Four-layer memory store + Qdrant. Opens iff
        // `[memory.qdrant]` is present with a non-empty URL OR
        // the operator set an explicit layered_db_path. The
        // store itself is cheap and additive; the Qdrant
        // ensure_collection happens later (post-rpc) so the
        // controller still boots when Qdrant is offline.
        let layered_ctx = open_layered_memory(&mem_cfg)?;
        if let Some(ctx) = &layered_ctx {
            tracing::info!(
                qdrant = ctx.qdrant.is_some(),
                "memory node: layered store online (Raw records mirrored from memory.write_turn)"
            );
        }
        // RELIX-7.15 memory PII: derive the per-node anonymizer.
        // The layered surface already builds one inside
        // open_layered_memory; if the layered store is disabled
        // we still need an anonymizer for the turns-table
        // path. Both end up reading the same `[memory.pii]`
        // config so the recorder + the embedder defensive
        // pass + the promoter pass + the manifest caps all
        // agree.
        let memory_anonymizer = layered_ctx
            .as_ref()
            .map(|ctx| ctx.anonymizer.clone())
            .unwrap_or_else(|| {
                std::sync::Arc::new(crate::training::PiiAnonymizer::from_config(&mem_cfg.pii))
            });
        crate::nodes::memory::register(
            bridge,
            store.clone(),
            curator_ai_cell.clone(),
            embedding_cell.clone(),
            embedding_model,
            curator_handler_cfg,
            layered_ctx.clone(),
            curator_coord_cell.clone(),
            memory_anonymizer.clone(),
        );
        // Spawn the curator scheduler iff [memory.curator] is
        // configured AND enabled. Discovery of the AI + coord
        // peers is deferred to post-rpc::Client setup; see
        // `StartupWiring::MemoryCurator`.
        if let Some(curator_cfg) = mem_cfg.curator.clone() {
            if curator_cfg.enabled {
                crate::nodes::memory::spawn_curator_scheduler(
                    store.clone(),
                    curator_state.clone(),
                    curator_ai_cell.clone(),
                    curator_coord_cell.clone(),
                    curator_cfg.clone(),
                );
            } else {
                tracing::info!(
                    "memory node: [memory.curator] enabled = false; scheduler not spawned"
                );
            }
            out.push(StartupWiring::MemoryCurator {
                ai_cell: curator_ai_cell,
                coord_cell: curator_coord_cell,
                state: curator_state,
                cfg: curator_cfg,
                embedding_cell: mem_cfg
                    .embedding_peer
                    .as_ref()
                    .map(|_| embedding_cell.clone()),
                embedding_cfg: mem_cfg.embedding_peer.clone(),
            });
        } else {
            tracing::info!("memory node: no [memory.curator] section; curator scheduler disabled");
            if let Some(epeer) = mem_cfg.embedding_peer.clone() {
                out.push(StartupWiring::MemoryEmbedding {
                    cell: embedding_cell.clone(),
                    cfg: epeer,
                });
            }
        }
        // Background bring-up of the Qdrant collection + the
        // embedding pipeline. Both are best-effort; failures
        // log a warn and don't keep the memory node from
        // starting. The pipeline's embed shim reads the
        // embedding dispatcher cell on every tick, so a
        // late-startup dispatcher just means the first few
        // ticks log "not configured" and then it starts
        // working — no second wiring path needed.
        if let Some(layered) = &layered_ctx {
            if let Some(q) = &layered.qdrant {
                let q = q.clone();
                tokio::spawn(async move {
                    match q.ensure_collection().await {
                        Ok(()) => {
                            tracing::info!("memory node: qdrant collection ensured at startup")
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            "memory node: qdrant ensure_collection failed; pipeline will retry on next upsert"
                        ),
                    }
                });
            }
            if let Some(emb_cfg) = mem_cfg.embedder.clone()
                && emb_cfg.enabled
            {
                let dispatcher_cell = embedding_cell.clone();
                let dispatcher_model = mem_cfg
                    .embedding_peer
                    .as_ref()
                    .map(|p| p.model.clone())
                    .unwrap_or_else(|| "text-embedding-3-small".to_string());
                let embed_fn: crate::nodes::memory::embedder::EmbedFn =
                    std::sync::Arc::new(move |texts: Vec<String>| {
                        let cell = dispatcher_cell.clone();
                        let model = dispatcher_model.clone();
                        Box::pin(async move {
                            let dispatcher = cell
                                .get()
                                .cloned()
                                .ok_or_else(|| "embedding dispatcher not configured".to_string())?;
                            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
                            dispatcher
                                .embed(&model, &refs)
                                .await
                                .map_err(|e| e.to_string())
                        })
                    });
                let pipeline =
                    crate::nodes::memory::embedder::EmbeddingPipeline::new_with_anonymizer(
                        layered.store.clone(),
                        layered.qdrant.clone(),
                        embed_fn,
                        emb_cfg.batch_size,
                        emb_cfg.interval_secs,
                        layered.anonymizer.clone(),
                    );
                pipeline.spawn();
                tracing::info!(
                    batch_size = emb_cfg.batch_size,
                    interval_secs = emb_cfg.interval_secs,
                    "memory node: embedding pipeline spawned"
                );
            }
        }
        // ── GAP 6: spawn the memory integrity auditor. Runs
        // every 24h; pure read pass over Layer 3 + Layer 4.
        if let Some(layered) = layered_ctx.as_ref() {
            let auditor =
                crate::nodes::memory::integrity::MemoryIntegrityAuditor::new(layered.store.clone());
            auditor.spawn();
            tracing::info!(
                interval_secs = crate::nodes::memory::integrity::DEFAULT_AUDIT_INTERVAL_SECS,
                "memory node: integrity auditor spawned"
            );
        }
        // ── GAP 8: spawn the consolidation archiver. Runs
        // every 6h; archives terminal Layer-3 observations and
        // marks their parent Raw rows as consolidated.
        if let Some(layered) = layered_ctx.as_ref() {
            let archiver = std::sync::Arc::new(
                crate::nodes::memory::archiver::ConsolidationArchiver::new(layered.store.clone()),
            );
            archiver.spawn();
            tracing::info!(
                interval_secs = crate::nodes::memory::archiver::DEFAULT_ARCHIVE_INTERVAL_SECS,
                "memory node: consolidation archiver spawned"
            );
        }
        let memory_caps: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "memory.write_turn",
                "Append one chat turn (role + text) to a session's memory.",
                &["persist", "memory"],
                &["mutate:memory"],
            ),
            (
                "memory.recent_for_session",
                "Read the N most recent turns for a session, oldest-first.",
                &["read", "memory"],
                &["reads:internal"],
            ),
            (
                "memory.search_turns",
                "FTS5 substring search across all stored chat turns. \
                 Was `memory.search` before the vector-memory landing; \
                 renamed so `memory.search` can be the semantic search \
                 over per-subject embeddings.",
                &["search", "memory", "fts"],
                &["reads:internal"],
            ),
            (
                "memory.embed",
                "Embed a memory chunk and store it in the per-subject \
                 vector store. Arg: subject_id|target|text. Returns \
                 `embedding_id=<id>\\n` or `ok|embedding_id=<id>\\n` on \
                 dedup. Requires [memory.embedding_peer] in the memory \
                 controller config.",
                &["persist", "memory", "embedding"],
                &["mutate:memory", "external:ai"],
            ),
            (
                "memory.search",
                "Semantic search over a subject's memory embeddings. \
                 Arg: subject_id|target|query[|limit] (default 5, max 20). \
                 Returns tab-separated rows `embedding_id\\tscore\\tchunk_text\\n` \
                 newest-first, then `count=N\\n`. Requires \
                 [memory.embedding_peer].",
                &["search", "memory", "embedding", "semantic"],
                &["reads:internal", "external:ai"],
            ),
            (
                "memory.embed_all",
                "Re-embed all existing persistent memory entries for a \
                 subject_id. Chunks are split on `§`. Dedupes \
                 already-embedded chunks via blake3(text). Returns \
                 `ok|chunks_embedded=N\\n`.",
                &["mutate", "memory", "embedding"],
                &["mutate:memory", "external:ai"],
            ),
            (
                "memory.agent_read",
                "Read persistent agent + user memory for a subject_id \
                 (frozen-snapshot pattern). Returns header `agent_bytes=N|user_bytes=M\\n` \
                 followed by the raw bytes.",
                &["read", "memory", "agent_memory"],
                &["reads:internal"],
            ),
            (
                "memory.agent_write",
                "Add / replace / remove / read one persistent memory \
                 target. Arg: subject_id|target|action|data. Targets: \
                 'agent' (cap 2200 chars) or 'user' (cap 1375 chars). \
                 Entries separated by `§`.",
                &["persist", "memory", "agent_memory"],
                &["mutate:memory"],
            ),
            (
                "memory.agent_curate",
                "Curator: read a subject's agent + user memory, \
                 ask the AI peer to consolidate / drop stale entries, \
                 write the result back. Arg: subject_id|ai_peer_alias. \
                 Returns pipe-delimited summary (chars before/after, \
                 entries before/after). Existing memory is preserved \
                 on any AI failure.",
                &["mutate", "memory", "agent_memory", "curate"],
                &["mutate:memory", "external:ai"],
            ),
            (
                "memory.curator_status",
                "Read the curator scheduler's live state — \
                 enabled, interval_secs, min_chars_to_curate, \
                 running, last_run_at, next_run_at, and the last \
                 run summary (agents_reviewed, agents_curated, \
                 total_chars_saved). Returns pipe-delimited \
                 key=value pairs. Pure read.",
                &["read", "memory", "curator", "status"],
                &["reads:internal"],
            ),
            (
                "memory.pii_scan",
                "RELIX-7.15: detect PII spans in arbitrary text. \
                 Args JSON `{text}`. Returns `{spans, count}`. \
                 Operators use this to audit what PII would be \
                 detected in a sample BEFORE flipping \
                 `[memory.pii] enabled = true`.",
                &["read", "memory", "pii"],
                &["reads:internal"],
            ),
            (
                "memory.anonymize_preview",
                "RELIX-7.15: preview what the memory PII \
                 anonymizer would output. Args JSON \
                 `{text, strategy?}`. `strategy` (`redact` / \
                 `pseudonymize` / `allow`) overrides the global \
                 `[memory.pii] strategy` for the preview only. \
                 Returns `{anonymized, spans}`.",
                &["read", "memory", "pii"],
                &["reads:internal"],
            ),
            (
                "memory.bulk_anonymize",
                "RELIX-7.15 migration: walk every row in the \
                 turns table AND the four-layer `memory_records` \
                 table, rewriting each through the configured \
                 anonymizer. Idempotent — re-running produces \
                 zero `changed` on already-clean rows. Returns \
                 per-table + per-layer (scanned, changed) \
                 counts. Refuses to run when `[memory.pii] \
                 enabled = false`. Args: empty JSON object.",
                &["mutate", "memory", "pii", "migration"],
                &["mutate:memory"],
            ),
            // ── GAP 5: missing memory capabilities ─────────────
            (
                "memory.dialectic",
                "GAP 5: Q&A across one subject's Layer 3/4 \
                 memory. Args JSON `{observer_id, subject_id, \
                 question}`. Loads the Layer-4 model if any, \
                 searches Layer-3 observations (Qdrant w/ text \
                 fallback), and asks the configured dialectic \
                 model to synthesise. Returns `{answer, \
                 confidence, sources_used, model_used, \
                 fallback_reason?}`.",
                &["read", "memory", "dialectic", "semantic"],
                &["reads:internal", "external:ai"],
            ),
            (
                "memory.ingest_document",
                "GAP 5: chunk + embed a document into Layer 2. \
                 Args JSON `{observer_id, subject_id, source, \
                 content?|content_base64?, content_type, \
                 chunk_size_chars?}`. Supports text, markdown, \
                 code, and pdf (lopdf). Returns counts of \
                 chunks_persisted / chunks_embedded / \
                 deferred_embeddings.",
                &["persist", "memory", "ingest", "embedding"],
                &["mutate:memory", "external:ai"],
            ),
            (
                "memory.ingest_image",
                "GAP 5: vision-embed an image into Layer 2. \
                 Args JSON `{observer_id, subject_id, source, \
                 image_data}` where image_data is base64 of a \
                 PNG/JPEG/PDF. PDFs are routed through the same \
                 lopdf pipeline as ingest_document. Returns \
                 `{records_persisted, embedded, \
                 deferred_embeddings, pdf_pages?}`.",
                &["persist", "memory", "ingest", "vision"],
                &["mutate:memory", "external:ai"],
            ),
            (
                "memory.context_flush",
                "GAP 5: explicit promotion of in-context conver\
                 sational turns into Layer 2. Args JSON \
                 `{session_id, agent_name, keep_recent_n?}` \
                 (default keep_recent_n = 5). Embeds and \
                 persists all but the keep_recent_n most recent \
                 unflushed turns and marks them flushed. Returns \
                 `{flushed_count, remaining_in_context, \
                 session_id, embedded, deferred_embeddings}`.",
                &["mutate", "memory", "ingest", "embedding"],
                &["mutate:memory", "external:ai"],
            ),
            // ── GAP 6: memory-poisoning defense quarantine ────
            (
                "memory.quarantine_list",
                "GAP 6: page through observation candidates the \
                 anomaly scorer parked in the quarantine table. \
                 Args JSON `{limit?, source?}` (default \
                 limit=50, max 500). Returns `{rows: [{id, \
                 source, text, reason, source_trust, \
                 queued_at_ms}], count}` newest-first.",
                &["read", "memory", "quarantine"],
                &["reads:internal"],
            ),
            (
                "memory.quarantine_approve",
                "GAP 6: promote a quarantined candidate to a \
                 real Layer-3 observation. Args JSON `{id}`. \
                 The candidate runs through the standard \
                 anonymizer before insert; tags include \
                 `origin:quarantine_approved`. Returns `{ok, \
                 observation_id}`.",
                &["mutate", "memory", "quarantine"],
                &["mutate:memory"],
            ),
            (
                "memory.quarantine_reject",
                "GAP 6: permanently discard a quarantined \
                 candidate. Args JSON `{id}`. The candidate is \
                 deleted with no audit-side trace beyond the \
                 chronicle reject event. Returns `{ok}`.",
                &["mutate", "memory", "quarantine"],
                &["mutate:memory"],
            ),
            // ── GAP 7: memory inspector editing surface ──────
            (
                "memory.edit_record",
                "GAP 7: replace one record's text. Args JSON \
                 `{id, text}`. The new text runs through the \
                 standard PII anonymizer before insert and the \
                 embedding pointer is cleared so the background \
                 pipeline re-embeds on its next tick. Returns \
                 `{ok, id, last_edited_ms}`.",
                &["mutate", "memory", "inspector"],
                &["mutate:memory"],
            ),
            (
                "memory.freeze_record",
                "GAP 7: pin a record so the curator never \
                 overwrites it, the context-flush archiver \
                 never invalidates it, and the consolidation \
                 pipeline never archives it. Args JSON `{id}`. \
                 Returns `{ok, id, frozen: true}`.",
                &["mutate", "memory", "inspector"],
                &["mutate:memory"],
            ),
            (
                "memory.unfreeze_record",
                "GAP 7: undo memory.freeze_record. Args JSON \
                 `{id}`. Returns `{ok, id, frozen: false}`.",
                &["mutate", "memory", "inspector"],
                &["mutate:memory"],
            ),
            (
                "memory.bulk_export",
                "GAP 7: export every record for one source as \
                 JSON. Args JSON `{source, layer?}` where layer \
                 is optional ('raw' / 'semantic' / \
                 'observation' / 'model'). Returns `{source, \
                 records, count}`.",
                &["read", "memory", "inspector", "export"],
                &["reads:internal"],
            ),
            (
                "memory.request_model_refresh",
                "GAP 7: force the next promoter tick to \
                 regenerate the Layer-4 model for one source. \
                 Args JSON `{source}`. Mechanism: age the \
                 latest model's observed_at past the throttle \
                 window. Returns `{ok, source, \
                 aged_existing_model}`.",
                &["mutate", "memory", "inspector", "promoter"],
                &["mutate:memory"],
            ),
        ];
        for (m, desc, cats, tags) in memory_caps {
            // PH-CAP-RISK: memory caps are either reads (Safe) or
            // writes to the per-task memory store (Low).
            let risk = if cats.contains(&"search") || cats.contains(&"read") {
                relix_core::capability::RiskLevel::Safe
            } else {
                relix_core::capability::RiskLevel::Low
            };
            manifest.add_capability(
                CapabilityDescriptor::unary(*m)
                    .with_description(*desc)
                    .with_categories(cats.iter().map(|s| (*s).into()))
                    .with_sensitivity(tags.iter().map(|s| (*s).into()))
                    .with_risk(risk),
            );
        }
        tracing::info!(
            db = %mem_cfg.db_path.display(),
            "memory node: registered memory.write_turn / memory.recent_for_session / memory.search"
        );

        // ── RELIX-7.16: agent-to-agent knowledge transfer ─────
        //
        // Five `knowledge.*` caps + an AutoShareTask, opt-in via
        // `[knowledge]` in the memory-node TOML. The caps need
        // the layered store so they're registered HERE (not on
        // the coordinator branch — the data lives on the
        // memory node). When `[knowledge]` is absent or the
        // group list is empty, no caps are registered and no
        // background task spawns.
        if let (Some(layered), Some(knowledge_cfg)) = (layered_ctx.as_ref(), cfg.knowledge.clone())
        {
            if knowledge_cfg.has_active_groups() {
                let svc = match crate::knowledge::KnowledgeService::new(
                    layered.store.clone(),
                    &knowledge_cfg,
                ) {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        return Err(format!("[knowledge] {e}").into());
                    }
                };
                // RELIX-7.16 GAP 3: attach local node name + the
                // shared MeshKnowledgeDispatcher cell BEFORE the
                // service is shared with the dispatch bridge. The
                // dispatcher is late-bound by a `StartupWiring`
                // entry below — until that fires, cross-node
                // shares reject with `Unreachable`.
                let local_node_name = cfg.controller.name.clone();
                let mesh_cell: Arc<
                    tokio::sync::OnceCell<Arc<dyn crate::knowledge::RemoteKnowledgeDispatcher>>,
                > = Arc::new(tokio::sync::OnceCell::new());
                let late_dispatcher: Arc<dyn crate::knowledge::RemoteKnowledgeDispatcher> =
                    Arc::new(crate::knowledge::remote::LateBoundDispatcher::new(
                        mesh_cell.clone(),
                    ));
                // Load the ed25519 signing key from the identity
                // key file so outbound `knowledge.accept_shared`
                // payloads are authenticated by the source node's
                // own key. If the key file is missing / malformed
                // we fall back to local-only sharing — a warn is
                // logged below.
                let signing_key_bytes: Option<[u8; 32]> =
                    match std::fs::read(&cfg.identity.key_path) {
                        Ok(b) if b.len() == 32 => {
                            let mut k = [0u8; 32];
                            k.copy_from_slice(&b);
                            Some(k)
                        }
                        Ok(_) => {
                            tracing::warn!(
                                key_path = %cfg.identity.key_path.display(),
                                "knowledge: identity key not 32 bytes; mesh-routed shares disabled"
                            );
                            None
                        }
                        Err(e) => {
                            tracing::warn!(
                                key_path = %cfg.identity.key_path.display(),
                                error = %e,
                                "knowledge: identity key unreadable; mesh-routed shares disabled"
                            );
                            None
                        }
                    };
                let mut svc_inner = (*svc).clone();
                svc_inner = svc_inner.with_local_node(local_node_name.clone());
                if let Some(key_bytes) = signing_key_bytes {
                    let signer = Arc::new(ed25519_dalek::SigningKey::from_bytes(&key_bytes));
                    svc_inner =
                        svc_inner.with_mesh(local_node_name.clone(), signer, late_dispatcher);
                }
                // SEC §16: populate the source-node key registry from
                // [knowledge_trust] so accept_shared binds each inbound
                // share's signature to the claimed source node by
                // default. With no keys configured (and no explicit
                // opt-out) inbound shares from unconfigured sources are
                // REJECTED — the binding is live, not dormant.
                let kt = cfg.knowledge_trust.clone().unwrap_or_default();
                let trusted_keys = kt
                    .parsed_keys()
                    .map_err(|e| format!("[knowledge_trust] {e}"))?;
                for (node, pubkey) in &trusted_keys {
                    svc_inner = svc_inner.with_source_node_key(node.clone(), *pubkey);
                }
                if kt.allow_unbound_sources {
                    tracing::warn!(
                        "knowledge: [knowledge_trust] allow_unbound_sources = true — inbound \
                         shares from sources with NO configured identity key are accepted on \
                         SIGNATURE ALONE; source-node binding is DISABLED for those sources \
                         (insecure, deliberate opt-out)"
                    );
                    svc_inner = svc_inner.with_allow_unbound_sources(true);
                } else if trusted_keys.is_empty() {
                    tracing::warn!(
                        "knowledge: no [knowledge_trust].source_nodes configured and \
                         allow_unbound_sources = false — inbound mesh shares from unconfigured \
                         sources will be REJECTED (source-node binding enforced/fail-closed). \
                         Configure each peer's identity pubkey to enable cross-node sharing."
                    );
                } else {
                    tracing::info!(
                        trusted_source_nodes = trusted_keys.len(),
                        "knowledge: source-node binding enforced for inbound shares (SEC §16)"
                    );
                }
                // RELIX-7.16 GAP 4: build the AutoShareTask
                // FIRST so we can grab its lifetime stats
                // handle, install it on the service that the
                // dispatch bridge sees, and STILL spawn the
                // same task. The handle is Arc-backed so
                // service-side reads and task-side writes
                // share storage.
                let autoshare_cfg =
                    crate::knowledge::AutoShareConfig::from_knowledge_config(&knowledge_cfg);
                let task = crate::knowledge::AutoShareTask::new(
                    svc_inner.clone(),
                    layered.store.clone(),
                    autoshare_cfg,
                );
                let lifetime_stats = task.lifetime_stats();
                svc_inner = svc_inner.with_autoshare_stats(lifetime_stats);
                let svc = Arc::new(svc_inner);
                crate::knowledge::register(bridge, svc.clone());
                for (method, doc) in crate::knowledge::knowledge_capability_descriptors() {
                    let cats: &[&str] = match *method {
                        "knowledge.share" | "knowledge.group_broadcast" => {
                            &["mutate", "memory", "knowledge"]
                        }
                        "knowledge.revoke" => &["mutate", "memory", "knowledge"],
                        "knowledge.recall" => &["mutate", "memory", "knowledge"],
                        "knowledge.accept_shared" => &["mutate", "memory", "knowledge"],
                        _ => &["read", "memory", "knowledge"],
                    };
                    manifest.add_capability(
                        CapabilityDescriptor::unary(*method)
                            .with_description(*doc)
                            .with_categories(cats.iter().map(|s| (*s).into())),
                    );
                }
                // Park the mesh cell + the peers map on a
                // StartupWiring entry so the run() loop can wire
                // it after the rpc::Client is up. The remote
                // dispatcher is keyed by node name; we map each
                // configured peer (peers.toml entry) to its
                // own MeshClient at fire time.
                out.push(StartupWiring::KnowledgeMesh {
                    cell: mesh_cell,
                    peers: cfg.peers.clone(),
                    deadline_secs: 30,
                    // SEC §17: hand the service's source-key registry to
                    // the discovery task so connecting peers' verified
                    // identity keys auto-populate it — no manual config.
                    source_key_registry: svc.source_node_key_registry(),
                });
                // Spawn the AutoShareTask. The handle is dropped
                // — the task runs for the process lifetime;
                // shutdown happens when tokio's runtime tears
                // down.
                let _handle = task.spawn();
                tracing::info!(
                    groups = knowledge_cfg.groups.len(),
                    auto_share_interval_secs = knowledge_cfg.auto_share_interval_secs,
                    "memory node: registered knowledge.* + spawned AutoShareTask"
                );
                // RELIX-7.16 GAP 1: spawn the MemoryQualityScorer
                // background loop iff `[knowledge.quality_scorer]`
                // is enabled. The handle is dropped — the task
                // runs for the process lifetime.
                if knowledge_cfg.quality_scorer.enabled {
                    let _q_handle = crate::knowledge::spawn_memory_quality_scorer(
                        layered.store.clone(),
                        knowledge_cfg.quality_scorer.clone(),
                    );
                    tracing::info!(
                        interval_secs = knowledge_cfg.quality_scorer.interval_secs,
                        batch_size = knowledge_cfg.quality_scorer.batch_size,
                        observation_baseline = knowledge_cfg.quality_scorer.observation_baseline,
                        "memory node: spawned MemoryQualityScorer"
                    );
                }
            } else {
                tracing::info!(
                    "memory node: [knowledge] section present but has no active groups; \
                     knowledge.* not registered"
                );
            }
        }
    }
    if cfg.controller.node_type == "ai" {
        let ai_cfg: crate::nodes::ai::AiConfig = match &cfg.ai {
            Some(raw) => raw
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| format!("[ai] parse: {e}"))?,
            None => crate::nodes::ai::AiConfig::default(),
        };
        let provider = crate::nodes::ai::build_provider(&ai_cfg)?;
        let provider_name = provider.provider_name();
        let default_model = ai_cfg.model.clone();
        // Frozen-snapshot memory cell. Always passed to
        // `ai::register`; the controller populates it later
        // (post-startup) iff `[ai.memory_peer]` is configured.
        // When the cell stays empty, `ai.chat` proceeds without
        // memory injection.
        let memory_cell: Arc<tokio::sync::OnceCell<Arc<dyn crate::nodes::ai::MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        // SOUL.md cache. `AgentConfig::None` means the cache is
        // a no-op (every `current()` returns None) so existing
        // controllers without `[ai.agent]` keep their prompt
        // composition unchanged. When operators set
        // `[ai.agent] name = "alice"` (or `soul_path`), the
        // cache resolves the soul once per call with mtime-
        // tracked reload — file edits take effect on the next
        // chat without a restart.
        let soul_cache = crate::nodes::ai::SoulCache::from_config(ai_cfg.agent.as_ref());
        // Skill library. Loaded once at startup from the
        // documented discovery roots; an empty library is a
        // no-op (no skill hint is ever prepended). Hot reload
        // is a follow-up — operators today restart the
        // controller to pick up new skills.
        let skills_cache = crate::nodes::ai::skills::SkillsCache::load(&[]);
        // Skill matcher prefers embedding-cosine similarity
        // when an embedding-capable provider is wired. The
        // matcher hands the provider directly (no libp2p hop)
        // via `ProviderEmbedAdapter` and lazily embeds the
        // skill catalogue on the first matching call. If the
        // provider doesn't support embeddings, the bulk-embed
        // returns Err and the matcher falls back to keyword
        // overlap silently.
        let embed_adapter: std::sync::Arc<dyn crate::nodes::ai::skills::SkillEmbedDispatcher> =
            std::sync::Arc::new(crate::nodes::ai::skills::ProviderEmbedAdapter(
                provider.clone(),
            ));
        let skill_matcher = crate::nodes::ai::skills::SkillMatcher::new(
            skills_cache,
            Some(embed_adapter),
            default_model.clone(),
            crate::nodes::ai::skills::SKILL_MATCH_THRESHOLD,
        );
        // Input guardrail. Parses `[guardrails.input]` from the
        // top-level config when present; absent / `enabled =
        // false` produces a permissive instance so existing
        // controllers behave exactly as before.
        let input_guardrail = build_input_guardrail(cfg);
        // W1: build a per-controller ToolDispatcher that wraps
        // the SecretStore + the shared AgentAccessBroker. The
        // dispatcher is the choke-point every planner-emitted
        // ToolCall flows through; admission failures surface as
        // structured errors in the chat response. W2: the same
        // broker is wired into the DispatchBridge admission
        // pipeline, so per-agent policies apply uniformly
        // whether the call enters via mesh dispatch or via the
        // AI handler's ToolDispatcher.
        let secret_store =
            std::sync::Arc::new(crate::nodes::execution::secrets::SecretStore::from_env());
        // GAP 11: parse the `[execution.gateway]` block and
        // build the persistent transaction store when an
        // operator opted in. Absent → in-memory ActionGateway
        // only (legacy behaviour); present + db_path missing →
        // warn + skip; present + db_path → open the store and
        // wire it into the dispatcher.
        let gateway_section = cfg
            .execution
            .as_ref()
            .and_then(|e| e.gateway.as_ref())
            .cloned();
        let gateway_store: Option<
            std::sync::Arc<crate::nodes::execution::transaction_store::TransactionStore>,
        > = match gateway_section.as_ref() {
            Some(g) if g.db_path.is_some() => {
                let p = g.db_path.as_ref().unwrap().clone();
                match crate::nodes::execution::transaction_store::TransactionStore::open(&p) {
                    Ok(s) => Some(std::sync::Arc::new(s)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = %p.display(),
                            "[execution.gateway] open store failed; running in-memory only",
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        let blocked_tools = gateway_section
            .as_ref()
            .map(|g| g.blocked_tools.clone())
            .unwrap_or_default();
        let global_dry_run = gateway_section.as_ref().map(|g| g.dry_run).unwrap_or(false);
        let mut tool_dispatcher_builder = crate::nodes::tool::dispatcher::ToolDispatcher::new(
            secret_store,
            access_broker.clone(),
        )
        .with_blocked_tools(blocked_tools)
        .with_global_dry_run(global_dry_run);
        if let Some(s) = gateway_store.clone() {
            tool_dispatcher_builder = tool_dispatcher_builder.with_transaction_store(s);
        }
        // GAP 12: build the evidence store next to the
        // transaction store. When `evidence_db_path` is set, use
        // it; otherwise we park evidence at
        // `<gateway_db_stem>-evidence.db` so a single backup
        // captures both stores. The store is opt-in: when the
        // operator did not configure a transaction store, no
        // evidence store opens either (capture without
        // transaction is meaningless).
        let evidence_store: Option<
            std::sync::Arc<crate::nodes::execution::evidence::EvidenceStore>,
        > = match gateway_section.as_ref() {
            Some(g) if gateway_store.is_some() => {
                let ev_path = g.evidence_db_path.clone().or_else(|| {
                    g.db_path.clone().map(|p| {
                        let stem = p
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("gateway")
                            .to_string();
                        let mut q = p.clone();
                        q.set_file_name(format!("{stem}-evidence.db"));
                        q
                    })
                });
                if let Some(p) = ev_path {
                    let anon = std::sync::Arc::new(crate::training::PiiAnonymizer::disabled());
                    match crate::nodes::execution::evidence::EvidenceStore::open(&p, anon) {
                        Ok(s) => Some(std::sync::Arc::new(s)),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                path = %p.display(),
                                "[execution.gateway] open evidence store failed; capture disabled",
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(store) = evidence_store.clone() {
            let sink: std::sync::Arc<dyn crate::nodes::tool::dispatcher::EvidenceCaptureSink> =
                store.clone();
            tool_dispatcher_builder = tool_dispatcher_builder.with_evidence_sink(sink);
            crate::nodes::execution::evidence::register(bridge, store);
            tracing::info!("execution caps: registered execution.evidence handler");
        }
        let tool_dispatcher = std::sync::Arc::new(tool_dispatcher_builder);
        // Register `execution.rollback` + `execution.transaction_get`
        // when the store is available. The compensating dispatcher
        // hooks back into the same ToolDispatcher (downcast through
        // a thin adapter that calls `dispatch_with_options`) so
        // Tier A rollbacks ride the same admission pipeline.
        if let Some(store) = gateway_store.clone() {
            let comp_disp: std::sync::Arc<
                dyn crate::nodes::execution::rollback::CompensatingDispatcher,
            > = std::sync::Arc::new(LocalCompensatingDispatcher::new(tool_dispatcher.clone()));
            crate::nodes::execution::rollback::register(bridge, store, Some(comp_disp));
            tracing::info!("execution caps: registered rollback + transaction_get handlers");
        }
        // OnceCell for the outbound tool dispatcher used by
        // the AI handler's planner ToolCall path. The
        // controller's startup flow can later populate this
        // cell with a real mesh-backed dispatcher; today the
        // cell stays empty and the runner falls back to the
        // admit-only path.
        let tool_mesh_cell: std::sync::Arc<
            tokio::sync::OnceCell<
                std::sync::Arc<dyn crate::nodes::ai::execution::ToolMeshDispatcher>,
            >,
        > = std::sync::Arc::new(tokio::sync::OnceCell::new());
        // GAP 4: build the shared skill runtime when `[skills]`
        // is enabled. The bundle holds the SkillStore (shared
        // with the cap registration below) and optionally the
        // SkillExtractor (post-`ai.chat` hook) and
        // SkillRefinementEngine (24h background task).
        let skills_runtime = build_skills_runtime(&cfg.skills, provider.clone(), &default_model);
        if let Some(rt) = skills_runtime.as_ref() {
            crate::nodes::ai::skill_caps::register(bridge, rt.store.clone());
            tracing::info!("skill caps: registered six memory.skill_* handlers");
            if let Some(refinement) = rt.refinement.clone() {
                refinement.spawn();
                tracing::info!("skill refinement: background task spawned");
            }
        }
        let skill_extractor = skills_runtime.as_ref().and_then(|rt| rt.extractor.clone());
        // GAP 13 + 14: build the AI-controller-side
        // `ObservabilityContext` when the operator enabled
        // `[observability.two_sink]`. The bridge already
        // carries its own; this one covers mesh-internal
        // `ai.chat` dispatches that never enter the bridge.
        let ai_observability = build_ai_observability(cfg);
        let soul_cache_for_obs = soul_cache.clone();
        if let Some(obs) = ai_observability.as_ref() {
            crate::nodes::ai::provenance_hooks::record_soul_provenance(obs, &soul_cache_for_obs);
        }
        crate::nodes::ai::register(
            bridge,
            provider.clone(),
            default_model.clone(),
            memory_cell.clone(),
            soul_cache,
            skill_matcher,
            input_guardrail,
            Some(tool_dispatcher),
            tool_mesh_cell,
            metrics.map(|b| b.sink.clone()),
            training.map(|b| b.sink.clone()),
            skill_extractor,
            ai_observability.clone(),
            // RELIX-7.29 PART 1: per-controller `[ai.routing]`
            // smart routing config. Absent / disabled keeps the
            // AI handler byte-identical to the pre-routing path.
            ai_cfg.routing.clone(),
            // RELIX-7.29 PART 2: per-controller
            // `[confidence.self_consistency]` adaptive sampling
            // config. The AI handler READS this even though it's
            // a confidence-section field — SC has to run inside
            // the AI handler because it needs the provider Arc
            // for parallel sample dispatch.
            cfg.confidence
                .as_ref()
                .and_then(|c| c.self_consistency.clone()),
            // RELIX-7.29 PART 2: shared SC stats counters. The
            // `confidence.self_consistency_stats` cap reads from
            // the same instance the AI handler writes to.
            confidence_bundle.as_ref().map(|b| b.sc_stats.clone()),
            // RELIX-7.29 PART 3: `[ai.belief_state]` LLM-driven
            // belief tracker config. Absent / disabled keeps the
            // AI handler byte-identical to its pre-belief
            // behaviour. The returned BeliefStateTracker is
            // currently ignored — the dispatch surface already
            // serves `belief.get` / `belief.reset` from the same
            // shared instance the AI handler holds.
            ai_cfg.belief_state.clone(),
            // RELIX-7.29 follow-up: cross-restart belief
            // persistence. Wired when the AI controller has a
            // local `[memory]` section with a layered store
            // path AND `[ai.belief_state] enabled = true`. The
            // tracker writes every belief list to a Layer-4
            // record under a deterministic id so beliefs
            // survive restarts. Absent on multi-process
            // deployments where memory lives on a separate node.
            build_belief_persistence_store(cfg, &ai_cfg),
            // RELIX-7.29 PART 4: `[ai.judge]` judge model config.
            // Absent / disabled keeps the AI handler byte-
            // identical to its pre-judge behaviour. The returned
            // JudgeRecorder is currently ignored — the dispatch
            // surface already serves `judge.recent_verdicts` /
            // `judge.stats` from the same shared instance.
            ai_cfg.judge.clone(),
            // RELIX-GAP-10 / §7.23 perception security two-stage
            // isolation config. Absent / `enabled = false`
            // keeps the `ai.perception_extract` cap registered
            // in the documented-disabled mode.
            ai_cfg.perception_security.clone(),
        );
        // Hand back to run() so the post-rpc::Client setup can
        // build a MemoryDispatcher into the cell when
        // ai_cfg.memory_peer is configured.
        out.push(StartupWiring::AiMemory {
            cell: memory_cell,
            cfg: ai_cfg.memory_peer.clone(),
        });
        // Carry the provider name as a sensitivity tag so consumers (bridge
        // `/v1/models`) can derive a model label without a second RPC.
        manifest.add_capability(
            CapabilityDescriptor::unary("ai.chat")
                .with_sensitivity([format!("provider:{provider_name}")])
                .with_description(
                    "Single-shot chat completion. Provider is selected via the AI \
                     node's [ai] config; this descriptor carries the provider name \
                     as a sensitivity tag.",
                )
                .with_categories(["generate".into(), "ai".into()])
                .with_environment_requirements([format!("provider:{provider_name}")])
                .with_risk(relix_core::capability::RiskLevel::Medium),
        );
        // RELIX-2 step 3: streaming variant of ai.chat. Same
        // pre-flight (guardrails / memory / soul / skills) but
        // pipes tokens from `generate_reply_stream` over the
        // `/relix/rpc/stream/1` substream instead of returning
        // a single response body. Operators needing inline
        // tool dispatch / planner / approval verdicts use the
        // unary `ai.chat` — the streaming variant skips that
        // pipeline by design (it's pure token streaming, not
        // an agentic planning surface).
        manifest.add_capability(
            CapabilityDescriptor::stream_out("ai.chat.stream")
                .with_sensitivity([format!("provider:{provider_name}")])
                .with_description(
                    "Streaming chat completion. Same args + pre-flight as ai.chat \
                     (guardrails, memory/RAG, soul, skills). Response is a sequence \
                     of token chunks over a /relix/rpc/stream/1 substream, terminated \
                     by End. Skips planner / tool dispatch / approval — use ai.chat \
                     for those flows.",
                )
                .with_categories(["generate".into(), "ai".into(), "streaming".into()])
                .with_environment_requirements([format!("provider:{provider_name}")])
                .with_risk(relix_core::capability::RiskLevel::Medium),
        );
        manifest.add_capability(
            CapabilityDescriptor::unary("ai.embed")
                .with_sensitivity([format!("provider:{provider_name}")])
                .with_description(
                    "Batch text embedding. Arg `model|text1§text2§…`; returns \
                     `model|base64(f32-le)|...`. Used by the memory node's \
                     vector search; mock provider returns deterministic 8-dim \
                     vectors so the pipeline works without a real key.",
                )
                .with_categories(["generate".into(), "ai".into(), "embedding".into()])
                .with_environment_requirements([format!("provider:{provider_name}")])
                .with_risk(relix_core::capability::RiskLevel::Low),
        );
        tracing::info!(
            provider = %provider_name,
            default_model = %default_model,
            "ai node: registered ai.chat / ai.embed"
        );
    }
    if cfg.controller.node_type == "coordinator" {
        let raw = cfg.coordinator.clone().ok_or_else(|| {
            "node_type=coordinator requires a [coordinator] section with db_path".to_string()
        })?;
        let coord_cfg: crate::nodes::coordinator::CoordinatorConfig = raw
            .try_into()
            .map_err(|e: toml::de::Error| format!("[coordinator] parse: {e}"))?;
        let store = std::sync::Arc::new(crate::nodes::coordinator::TaskStore::open(&coord_cfg)?);
        // C1b: startup recovery scan. Promotes any task left in
        // `running` past its `max_runtime_secs` to `interrupted` and
        // appends a `task.interrupted` event explaining why. Tasks
        // without a deadline are left untouched.
        if coord_cfg.recovery_scan {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            match store.recover_interrupted(now) {
                Ok(ids) if !ids.is_empty() => tracing::warn!(
                    recovered = ids.len(),
                    "coordinator startup: marked stale `running` tasks as `interrupted`"
                ),
                Ok(_) => tracing::info!("coordinator startup: recovery scan found no stale tasks"),
                Err(e) => tracing::error!(error = %e, "coordinator startup: recovery scan failed"),
            }
            // Run-ledger recovery: every `brief_runs` row still `running` at
            // boot is stale — its child process died with the previous
            // coordinator (the in-memory handle is gone). Mark it
            // `interrupted`, record a `recovered` event, and release its Brief
            // Claim so the work can be re-dispatched. The live set is empty on
            // a fresh boot and `min_age_secs = 0`, so this reconciles ALL
            // leftover running runs.
            match store.recover_stale_runs(&std::collections::HashSet::new(), 0) {
                Ok(ids) if !ids.is_empty() => tracing::warn!(
                    recovered = ids.len(),
                    "coordinator startup: recovered stale `running` brief runs (no live child process)"
                ),
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(error = %e, "coordinator startup: run-ledger recovery failed")
                }
            }
        }
        // Background chronicle retention. Only spawns when
        // `[coordinator.retention] enabled = true`; the dry-run
        // surface via `task.compact_events` is unaffected. See
        // `docs/chronicle-retention.md` for the full design.
        if coord_cfg.retention.enabled {
            let retention_store = store.clone();
            let retention_cfg = coord_cfg.retention.clone();
            tokio::spawn(async move {
                run_retention_loop(retention_store, retention_cfg).await;
            });
            tracing::info!(
                interval_h = coord_cfg.retention.compact_interval_h,
                max_age_days = coord_cfg.retention.max_task_age_days,
                max_passes_per_run = coord_cfg.retention.max_passes_per_run,
                "coordinator startup: chronicle retention loop spawned"
            );
        }
        // Scheduled summary reports. Parsed from the top-level
        // `[reports]` section so operators can configure cadence +
        // delivery channels without touching the coordinator
        // section. Source pulls real numbers from the same
        // TaskStore; channel dispatch is wired separately by the
        // operator's channel peers (an empty channel list means
        // the loop assembles + logs but doesn't send — useful for
        // dry-run validation).
        let reports_cfg: crate::nodes::channels::reports::ReportsConfig = match &cfg.reports {
            Some(raw) => raw
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| format!("[reports] parse: {e}"))?,
            None => crate::nodes::channels::reports::ReportsConfig::default(),
        };
        if reports_cfg.enabled {
            let source: std::sync::Arc<dyn crate::nodes::channels::reports::ReportSource> =
                std::sync::Arc::new(
                    crate::nodes::channels::reports::CoordinatorReportSource::new(store.clone()),
                );
            crate::nodes::channels::reports::spawn_report_loop(
                reports_cfg.clone(),
                source,
                Vec::new(),
            );
            tracing::info!(
                schedule = %reports_cfg.schedule,
                channels = ?reports_cfg.channels,
                "coordinator startup: scheduled report loop spawned"
            );
        }
        // `[skills]` is parsed on the coordinator boot path so
        // the post-completion hook in `task.update` can mint
        // SKILL.md auto-skills. Absent ⇒ `None` ⇒ hook stays
        // dormant.
        let auto_skill_cfg = match &cfg.skills {
            Some(raw) => match raw
                .clone()
                .try_into::<crate::nodes::ai::skills::SkillsConfig>()
            {
                Ok(c) if c.auto_generate => Some(std::sync::Arc::new(c)),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "[skills] parse failed; auto-generation disabled");
                    None
                }
            },
            None => None,
        };
        // `[guardrails.drift]` is parsed on the coordinator
        // boot path so the post-update hook can evaluate
        // running tasks. Absent / disabled ⇒ `None` ⇒ hook
        // stays dormant.
        let drift_cfg = build_drift_config(cfg).map(std::sync::Arc::new);
        // W4: optional embedding dispatcher for the drift
        // hook. Today the coordinator boots without an
        // outbound mesh client (that lives on the
        // `relix-controller` binary). The cell stays empty
        // until the controller's post-startup wiring populates
        // it with a `MeshDriftEmbedDispatcher`. Empty cell
        // means the hook records `similarity=none` — honest
        // about the absent comparison.
        let drift_embedder_cell: crate::nodes::ai::guardrails::DriftEmbedDispatcherCell =
            std::sync::Arc::new(tokio::sync::OnceCell::new());
        let drift_embedder_cell_for_startup = drift_embedder_cell.clone();
        let agent_store = std::sync::Arc::new(
            crate::nodes::coordinator::agent::AgentStore::open(&coord_cfg.db_path)
                .map_err(|e| format!("[coordinator] agent store open: {e}"))?,
        );
        crate::nodes::coordinator::register(
            bridge,
            store.clone(),
            Some(agent_store.clone()),
            auto_skill_cfg,
            drift_cfg.clone(),
            drift_embedder_cell,
        );
        // PHASE 1 (spine): the Mandate + Campaign objects live in
        // the coordinator DB alongside the Brief (task) ledger.
        // Open the SpineStore and register the `mandate.*` /
        // `campaign.*` capabilities. Non-fatal: a spine-open
        // failure logs and leaves the caps unregistered rather
        // than aborting coordinator boot (the Brief ledger, which
        // already opened the same path above, keeps working).
        let spine_store_for_agent_caps = match crate::nodes::coordinator::spine::SpineStore::open(
            &coord_cfg.db_path,
        ) {
            Ok(spine_store) => {
                let spine_store = std::sync::Arc::new(spine_store);
                // OBJECT-LEVEL billing-code inheritance (company-model §6.6):
                // inject the spine as the Brief ledger's resolver so run
                // stamping can fall back Brief → ancestor Brief →
                // Campaign/Mandate → Guild, all within the Brief's own Guild.
                store.set_object_billing_resolver(spine_store.clone());
                crate::nodes::coordinator::spine::handlers::register(bridge, spine_store.clone());
                tracing::info!(
                    "coordinator startup: spine (mandate/campaign) capabilities registered"
                );
                Some(spine_store)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "coordinator startup: SpineStore open failed; mandate/campaign caps NOT registered"
                );
                None
            }
        };
        // RELIX-7.30 PART 1: out-of-band approval delivery
        // matrix. Wired when `[approval.delivery]` is present;
        // absent keeps the bridge on the pre-7.30 admission
        // path with no delivery store and no
        // `approval.delivery_status` cap.
        //
        // The constructed service is kept in scope so the
        // RELIX-7.18 research pipeline can reuse it (instead
        // of opening a second delivery store on the same path).
        let mut research_approval: Option<crate::approval::ApprovalDeliveryService> = None;
        if let Some(approval_section) = cfg.approval.as_ref()
            && let Some(delivery_cfg) = approval_section.delivery.clone()
        {
            let store_path = approval_section
                .delivery_db_path
                .clone()
                .unwrap_or_else(|| {
                    let mut p = coord_cfg.db_path.clone();
                    p.set_file_name("approval_delivery.db");
                    p
                });
            match crate::approval::ApprovalRequestStore::open(&store_path) {
                Ok(delivery_store) => {
                    let matrix = crate::approval::ApprovalDeliveryMatrix::new(delivery_cfg);
                    // PART 8: matrix validation — log any issue so the
                    // operator sees the problem at startup rather than
                    // at the first approval. The matrix still wires
                    // (with the dashboard fallback) so the controller
                    // boots; the operator gets a hard signal in logs.
                    for issue in matrix.validate() {
                        tracing::warn!(
                            issue = %issue,
                            "approval delivery: matrix validation issue"
                        );
                    }
                    // PART 8: build the multi-channel router. The
                    // dashboard slot is always in-process; the four
                    // remote slots dispatch via the shared
                    // `AlertMeshCell` to `<channel>.approval_send` on
                    // the operator-configured peer. The mesh cell is
                    // populated post-startup by the `CoordAlertMesh`
                    // wiring; until then the remote slots return
                    // `Disabled("mesh client not initialised")` and
                    // the dispatcher fails-soft on those approvals
                    // (logged in `delivery_failed`).
                    let cfg_channels = matrix.config().channels.clone();
                    let mut multi = crate::approval::MultiChannelDispatch::new().with_channel(
                        crate::approval::ChannelKind::Dashboard,
                        std::sync::Arc::new(crate::approval::DashboardChannelDispatch::enabled()),
                    );
                    // Reuse the metrics fan-out's mesh cell so operators
                    // don't have to wire two cells. When metrics is
                    // absent the cell is freshly minted — the remote
                    // slots will then permanently `Disabled` since
                    // nothing populates it, but the dashboard slot keeps
                    // working.
                    let alert_mesh_cell_for_approval: crate::metrics::AlertMeshCell = metrics
                        .map(|m| m.alert_mesh_cell.clone())
                        .unwrap_or_else(|| std::sync::Arc::new(tokio::sync::OnceCell::new()));
                    if let Some(tg) = cfg_channels.telegram.as_ref()
                        && tg.enabled
                    {
                        multi = multi.with_channel(
                            crate::approval::ChannelKind::Telegram,
                            std::sync::Arc::new(crate::approval::MeshSingleChannelDispatch::new(
                                alert_mesh_cell_for_approval.clone(),
                                tg.peer.clone(),
                                crate::approval::ChannelKind::Telegram,
                                tg.chat_id.clone(),
                                String::new(),
                            )),
                        );
                    }
                    if let Some(sl) = cfg_channels.slack.as_ref()
                        && sl.enabled
                    {
                        multi = multi.with_channel(
                            crate::approval::ChannelKind::Slack,
                            std::sync::Arc::new(crate::approval::MeshSingleChannelDispatch::new(
                                alert_mesh_cell_for_approval.clone(),
                                sl.peer.clone(),
                                crate::approval::ChannelKind::Slack,
                                sl.channel_id.clone(),
                                String::new(),
                            )),
                        );
                    }
                    if let Some(dc) = cfg_channels.discord.as_ref()
                        && dc.enabled
                    {
                        multi = multi.with_channel(
                            crate::approval::ChannelKind::Discord,
                            std::sync::Arc::new(crate::approval::MeshSingleChannelDispatch::new(
                                alert_mesh_cell_for_approval.clone(),
                                dc.peer.clone(),
                                crate::approval::ChannelKind::Discord,
                                dc.channel_id.clone(),
                                String::new(),
                            )),
                        );
                    }
                    if let Some(em) = cfg_channels.email.as_ref()
                        && em.enabled
                    {
                        multi = multi.with_channel(
                            crate::approval::ChannelKind::Email,
                            std::sync::Arc::new(crate::approval::MeshSingleChannelDispatch::new(
                                alert_mesh_cell_for_approval.clone(),
                                em.peer.clone(),
                                crate::approval::ChannelKind::Email,
                                em.to.clone(),
                                em.reply_to.clone(),
                            )),
                        );
                    }
                    let configured_count = multi.configured_channel_count();
                    let dispatch: std::sync::Arc<dyn crate::approval::ChannelDispatch> =
                        std::sync::Arc::new(multi);
                    // NOT-DONE 1: thread the dispatch bridge's
                    // clock through the §7.30 delivery service
                    // so escalation_at_ms / delivered_at_ms /
                    // decided_at_ms stamps observe the same
                    // time source as the admission gate.
                    let delivery_clock = bridge.clock();
                    let service = crate::approval::ApprovalDeliveryService::new_with_clock(
                        matrix,
                        delivery_store,
                        dispatch,
                        delivery_clock,
                    );
                    tracing::info!(
                        configured_channels = configured_count,
                        "approval delivery: wired MultiChannelDispatch (PART 8)"
                    );
                    crate::approval::caps::register(bridge, service.clone());
                    research_approval = Some(service);
                    for (method, doc) in [
                        (
                            "approval.delivery_status",
                            "Read the delivery state for one approval id.",
                        ),
                        (
                            "approval.deliver",
                            "Dispatch an approval request through the configured channel.",
                        ),
                        (
                            "approval.record_decision",
                            "Record an operator decision so the escalation timer can stand down.",
                        ),
                        (
                            "approval.failed_deliveries",
                            "List rows in delivery_failed status for operator reconciliation (PART 6).",
                        ),
                        (
                            "approval.list_pending",
                            "List rows in pending status for the dashboard surface (PART 5).",
                        ),
                    ] {
                        manifest.add_capability(
                            CapabilityDescriptor::unary(method)
                                .with_description(doc)
                                .with_categories(
                                    if matches!(
                                        method,
                                        "approval.delivery_status"
                                            | "approval.failed_deliveries"
                                            | "approval.list_pending"
                                    ) {
                                        ["read", "approval"]
                                    } else {
                                        ["mutate", "approval"]
                                    }
                                    .iter()
                                    .map(|s| (*s).into()),
                                ),
                        );
                    }
                    tracing::info!(
                        path = %store_path.display(),
                        "approval delivery matrix online (RELIX-7.30 PART 1)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = %store_path.display(),
                        error = %e,
                        "approval delivery store open failed; caps NOT registered"
                    );
                }
            }
        }
        // RELIX-7.30 PART 2: credential vault. Opens iff
        // `[credentials] enabled = true` AND at least one
        // configured key version's env var is set. Spawns the
        // rotation scheduler when both are wired.
        if let Some(cred_cfg) = cfg.credentials.clone()
            && cred_cfg.enabled
        {
            let key_versions = cred_cfg.key_versions_resolved();
            if key_versions.is_empty() {
                tracing::warn!(
                    env_var = %cred_cfg.master_key_env,
                    "credentials: no usable key versions (master_key_env unset and \
                     [credentials.key_versions] empty); vault NOT registered"
                );
            } else {
                let path = cred_cfg.db_path.clone().unwrap_or_else(|| {
                    let mut p = coord_cfg.db_path.clone();
                    p.set_file_name("credentials.db");
                    p
                });
                match crate::credentials::CredentialStore::open_with_params(
                    &path,
                    key_versions,
                    cred_cfg.kdf_params(),
                    false,
                ) {
                    Ok(store) => {
                        crate::credentials::caps::register(
                            bridge,
                            store.clone(),
                            Some(agent_store.clone()),
                        );
                        let notifier: std::sync::Arc<dyn crate::credentials::RotationNotifier> =
                            std::sync::Arc::new(crate::credentials::scheduler::LogRotationNotifier);
                        let scheduler = crate::credentials::RotationScheduler::new(
                            store,
                            notifier,
                            crate::credentials::RotationSchedulerConfig {
                                check_interval_secs: cred_cfg.rotation_check_interval_secs,
                            },
                        );
                        scheduler.spawn();
                        for (method, doc, mutate) in [
                            (
                                "credentials.store",
                                "Store + encrypt a credential value.",
                                true,
                            ),
                            (
                                "credentials.get",
                                "Decrypt a credential when caller is the owner.",
                                false,
                            ),
                            (
                                "credentials.rotate",
                                "Replace a credential value + bump version.",
                                true,
                            ),
                            ("credentials.revoke", "Mark a credential revoked.", true),
                            (
                                "credentials.list",
                                "List credential summaries (no values).",
                                false,
                            ),
                            (
                                "credentials.audit",
                                "Read the audit trail for one credential.",
                                false,
                            ),
                        ] {
                            manifest.add_capability(
                                CapabilityDescriptor::unary(method)
                                    .with_description(doc)
                                    .with_categories(
                                        if mutate {
                                            ["mutate", "credentials"]
                                        } else {
                                            ["read", "credentials"]
                                        }
                                        .iter()
                                        .map(|s| (*s).into()),
                                    ),
                            );
                        }
                        tracing::info!(
                            path = %path.display(),
                            check_interval_secs = cred_cfg.rotation_check_interval_secs,
                            "credentials vault online (RELIX-7.30 PART 2)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "credentials vault open failed; caps NOT registered"
                        );
                    }
                }
            }
        }
        // RELIX-7.30 PART 3: session-identity token service.
        // Wired when `[identity.session] enabled = true` AND
        // the signing-key env var is set with at least 32
        // bytes of entropy.
        if let Some(id_section) = cfg.session_identity.as_ref()
            && let Some(sess_cfg) = id_section.session.clone()
            && sess_cfg.enabled
        {
            // SEC PART 2: wrap the env-sourced key material in
            // Zeroizing so the String backing it is wiped as
            // soon as this scope ends (the service stores its
            // own zeroizing copy of the bytes).
            let key_material: zeroize::Zeroizing<String> = zeroize::Zeroizing::new(
                std::env::var(&sess_cfg.signing_key_env).unwrap_or_default(),
            );
            if key_material.len() < 32 {
                tracing::warn!(
                    env_var = %sess_cfg.signing_key_env,
                    got = key_material.len(),
                    "identity: signing key env unset or shorter than 32 bytes; service NOT registered"
                );
            } else {
                let path = sess_cfg.db_path.clone().unwrap_or_else(|| {
                    let mut p = coord_cfg.db_path.clone();
                    p.set_file_name("session_tokens.db");
                    p
                });
                match crate::identity::TokenStore::open(&path) {
                    Ok(store) => {
                        // NOT-DONE 1: thread the dispatch
                        // bridge's clock through the session
                        // identity service so issue / verify /
                        // idle-sweep stamp + compare against the
                        // same time source as the admission
                        // gate.
                        let session_clock = bridge.clock();
                        match crate::identity::SessionIdentityService::new_with_clock(
                            store,
                            sess_cfg.clone(),
                            key_material.as_bytes().to_vec(),
                            session_clock,
                        ) {
                            Ok(service) => {
                                crate::identity::caps::register(bridge, service.clone());
                                if sess_cfg.session_idle_timeout_secs > 0 {
                                    service.clone().spawn_idle_sweeper();
                                }
                                // P5: wire the session identity
                                // service into the dispatch
                                // bridge so the
                                // `verify_on_dispatch` gate has
                                // something to call. The flag
                                // is honoured in admission
                                // step 6 of both unary and
                                // streaming paths.
                                bridge.set_session_service(std::sync::Arc::new(service.clone()));
                                bridge.set_verify_on_dispatch(sess_cfg.verify_on_dispatch);
                                if sess_cfg.verify_on_dispatch {
                                    tracing::info!(
                                        "Session token verification enabled on dispatch. \
                                         Every capability call requires a valid session token."
                                    );
                                } else {
                                    tracing::warn!(
                                        "Session token verification is DISABLED. \
                                         Capability calls are not authenticated by session tokens. \
                                         Set [identity.session] verify_on_dispatch = true for production deployments."
                                    );
                                }
                                for (method, doc, mutate) in [
                                    (
                                        "identity.issue_token",
                                        "Issue a signed session token.",
                                        true,
                                    ),
                                    ("identity.verify_token", "Verify a session token.", false),
                                    ("identity.revoke_token", "Revoke a session's tokens.", true),
                                    (
                                        "identity.active_tokens",
                                        "List active session tokens.",
                                        false,
                                    ),
                                ] {
                                    manifest.add_capability(
                                        CapabilityDescriptor::unary(method)
                                            .with_description(doc)
                                            .with_categories(
                                                if mutate {
                                                    ["mutate", "identity"]
                                                } else {
                                                    ["read", "identity"]
                                                }
                                                .iter()
                                                .map(|s| (*s).into()),
                                            ),
                                    );
                                }
                                tracing::info!(
                                    path = %path.display(),
                                    ttl_secs = sess_cfg.session_ttl_secs,
                                    idle_timeout_secs = sess_cfg.session_idle_timeout_secs,
                                    "session identity service online (RELIX-7.30 PART 3)"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "identity: service construction failed; caps NOT registered"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "session-token store open failed; caps NOT registered"
                        );
                    }
                }
            }
        }
        // GATE 1 — FAIL CLOSED. If the operator asked for
        // `verify_on_dispatch = true` but the session
        // verification service did not wire up (section absent,
        // `enabled = false`, signing key missing/short, or store
        // / service init failed), refuse to boot with a specific
        // diagnostic rather than silently admitting every call
        // unverified. `verify_on_dispatch` is read from config
        // directly so this fires even when `enabled = false`
        // (which skips the wiring block entirely).
        {
            let session_sub = cfg
                .session_identity
                .as_ref()
                .and_then(|s| s.session.as_ref());
            let verify_requested = session_sub.map(|sc| sc.verify_on_dispatch).unwrap_or(false);
            let enabled = session_sub.map(|sc| sc.enabled).unwrap_or(false);
            let section_present = session_sub.is_some();
            let (signing_key_env, signing_key_len) = match session_sub {
                Some(sc) => (
                    sc.signing_key_env.clone(),
                    std::env::var(&sc.signing_key_env)
                        .map(|v| v.len())
                        .unwrap_or(0),
                ),
                None => (String::new(), 0),
            };
            session_verification_boot_gate(
                verify_requested,
                bridge.session_service_wired(),
                section_present,
                enabled,
                &signing_key_env,
                signing_key_len,
            )?;
        }
        // RELIX-7.18 / GAP 17 PART 2: research-backed identity
        // pipeline. Wired when `[session_identity.research]
        // enabled = true` AND `[ai]` is present AND a search
        // provider key resolves (Tavily / Brave / Perplexity).
        // Memory writes land on a LayeredMemoryStore opened on
        // the coordinator's sidecar (or the operator-specified
        // path), SQLite WAL handling concurrent writes if the
        // memory node holds the same file open.
        if let Some(id_section) = cfg.session_identity.as_ref()
            && let Some(research_cfg) = id_section.research.clone()
            && research_cfg.enabled
        {
            let ai_cfg: crate::nodes::ai::AiConfig = match &cfg.ai {
                Some(raw) => raw.clone().try_into().map_err(|e: toml::de::Error| {
                    format!("[ai] parse for identity.research: {e}")
                })?,
                None => crate::nodes::ai::AiConfig::default(),
            };
            let chat_provider = match crate::nodes::ai::build_provider(&ai_cfg) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "identity.research: AI provider build failed; cap NOT registered"
                    );
                    None
                }
            };
            let search_cfg = id_section.web_search.clone().unwrap_or_default();
            let search_provider = if search_cfg.enabled {
                match crate::nodes::tool::web_search::build_provider_from_env(&search_cfg) {
                    Ok(Some(p)) => Some(p),
                    Ok(None) => {
                        tracing::warn!(
                            "identity.research: no search provider key resolved \
                             (set TAVILY_API_KEY, BRAVE_SEARCH_API_KEY, or \
                             PERPLEXITY_API_KEY); cap NOT registered"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "identity.research: search-provider construction failed; \
                             cap NOT registered"
                        );
                        None
                    }
                }
            } else {
                tracing::warn!(
                    "identity.research: [session_identity.web_search] enabled = false; \
                     cap NOT registered"
                );
                None
            };
            let memory_store = if let Some(mem_raw) = cfg.memory.clone() {
                let mem_cfg: crate::nodes::memory::MemoryConfig = match mem_raw.try_into() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "identity.research: [memory] parse failed; \
                             cap NOT registered"
                        );
                        crate::nodes::memory::MemoryConfig::default()
                    }
                };
                match open_layered_memory(&mem_cfg) {
                    Ok(Some(ctx)) => Some(ctx.store.clone()),
                    Ok(None) => {
                        tracing::warn!(
                            "identity.research: layered memory not configured; \
                             cap NOT registered"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "identity.research: layered memory open failed; \
                             cap NOT registered"
                        );
                        None
                    }
                }
            } else {
                tracing::warn!("identity.research: [memory] section absent; cap NOT registered");
                None
            };
            if let (Some(chat), Some(search), Some(memory)) =
                (chat_provider, search_provider, memory_store)
            {
                if research_cfg.require_approval && research_approval.is_none() {
                    tracing::warn!(
                        "identity.research: require_approval = true but \
                         [approval.delivery] is unwired; cap NOT registered"
                    );
                } else {
                    let pipeline = crate::identity::research::ResearchPipeline::new(
                        research_cfg.clone(),
                        chat,
                        ai_cfg.model.clone(),
                        search,
                        research_approval.clone(),
                        Some(memory),
                    );
                    crate::identity::research_caps::register(bridge, pipeline);
                    manifest.add_capability(
                        CapabilityDescriptor::unary("identity.research")
                            .with_description(
                                "Research a subject via web search + LLM synthesis, \
                                 with optional human approval, and persist the \
                                 IdentityProfile to layered memory.",
                            )
                            .with_categories(
                                ["mutate", "identity", "research"]
                                    .iter()
                                    .map(|s| (*s).into()),
                            ),
                    );
                    tracing::info!(
                        require_approval = research_cfg.require_approval,
                        max_queries = research_cfg.max_queries,
                        max_results_per_query = research_cfg.max_results_per_query,
                        "identity.research pipeline online (RELIX-7.18 / GAP 17)"
                    );
                }
            }
        }
        // Park the cell + the coord_cfg ai-peer alias on
        // StartupWiring so the run() loop can populate it after
        // the rpc::Client is up. Falls back to the canonical
        // alias `"ai"` when the operator hasn't configured
        // a custom alias.
        if drift_cfg.as_ref().is_some_and(|c| c.enabled)
            && let Some(ai_peer_cfg) = coord_cfg.ai_peer.clone()
        {
            out.push(StartupWiring::CoordDriftEmbed {
                cell: drift_embedder_cell_for_startup,
                cfg: ai_peer_cfg,
            });
        }
        // Cron scheduler shares the coordinator's database.
        // Opens its own rusqlite connection against the same
        // file; SQLite handles cross-connection locking.
        let cron_store = std::sync::Arc::new(
            crate::nodes::coordinator::cron::CronStore::open(&coord_cfg.db_path)
                .map_err(|e| format!("[coordinator] cron store open: {e}"))?,
        );
        crate::nodes::coordinator::cron::register(bridge, cron_store.clone());
        let cron_caps: &[(&str, &str, &[&str])] = &[
            (
                "cron.create",
                "Create a scheduled job. Arg: name|schedule|flow_template|prompt|subject_id.",
                &["cron", "persist"],
            ),
            (
                "cron.list",
                "List cron jobs (filtered by subject_id; empty arg = all jobs).",
                &["cron", "read"],
            ),
            (
                "cron.get",
                "Read one cron job (every column).",
                &["cron", "read"],
            ),
            (
                "cron.update",
                "Update one of {enabled, schedule, prompt} on a cron job.",
                &["cron", "mutate"],
            ),
            (
                "cron.delete",
                "Permanently delete a cron job row.",
                &["cron", "mutate"],
            ),
        ];
        for (method, doc, cats) in cron_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }

        // Cron scheduler — optional [coordinator.cron] section.
        // The AI dispatcher cell is shared by both the periodic
        // tick AND the `cron.trigger` handler so manual + scheduled
        // fires use the same outbound client.
        let cron_sched_cfg_value = cfg
            .coordinator
            .as_ref()
            .and_then(|v| v.get("cron").cloned());
        let cron_sched_cfg: crate::nodes::coordinator::cron::CronSchedulerConfig =
            match cron_sched_cfg_value {
                Some(raw) => raw
                    .try_into()
                    .map_err(|e: toml::de::Error| format!("[coordinator.cron] parse: {e}"))?,
                None => crate::nodes::coordinator::cron::CronSchedulerConfig::default(),
            };
        let cron_ai_cell: crate::nodes::coordinator::cron::CronAiDispatcherCell =
            Arc::new(tokio::sync::OnceCell::new());
        // Register cron.trigger now so it's available even when
        // the scheduler loop is disabled — operators can still
        // run jobs manually.
        crate::nodes::coordinator::cron::register_trigger(
            bridge,
            store.clone(),
            cron_store.clone(),
            cron_ai_cell.clone(),
            cron_sched_cfg.max_job_secs,
        );
        manifest.add_capability(
            CapabilityDescriptor::unary("cron.trigger")
                .with_description(
                    "Manually fire a cron job. Creates a coordinator task with \
                     title `cron:<job_name>` and origin_surface=`scheduler`, \
                     records the fire on the cron row, and dispatches ai.chat \
                     in the background.",
                )
                .with_categories(["cron".into(), "mutate".into()]),
        );

        // Scheduler loop — spawned only when [coordinator.cron]
        // enabled = true (the default when the section exists).
        if cfg
            .coordinator
            .as_ref()
            .is_some_and(|v| v.get("cron").is_some())
            && cron_sched_cfg.enabled
        {
            crate::nodes::coordinator::cron::spawn_cron_scheduler(
                store.clone(),
                cron_store.clone(),
                cron_ai_cell.clone(),
                cron_sched_cfg.clone(),
            );
            tracing::info!(
                tick_secs = cron_sched_cfg.tick_secs,
                max_concurrent = cron_sched_cfg.max_concurrent,
                max_job_secs = cron_sched_cfg.max_job_secs,
                "coordinator node: cron scheduler spawned"
            );
            // Post-startup population of the AI cell — same
            // pattern as the memory curator. Spawns a task that
            // dials the configured AI peer and publishes a
            // CronAiMeshDispatcher into the cell.
            if let Some(ai_peer) = cron_sched_cfg.ai_peer.clone() {
                let key_path = cfg.identity.key_path.clone();
                let cell = cron_ai_cell.clone();
                tokio::spawn(async move {
                    populate_cron_ai_cell(cell, ai_peer, key_path).await;
                });
            } else {
                tracing::info!(
                    "coordinator: no [coordinator.cron.ai_peer]; cron AI dispatch disabled"
                );
            }
        } else {
            tracing::info!(
                "coordinator: cron scheduler not enabled ([coordinator.cron] missing or enabled=false)"
            );
        }

        tracing::info!(
            db = %coord_cfg.db_path.display(),
            "coordinator node: registered cron.create / list / get / update / delete / trigger"
        );

        // ── Workflow engine — RELIX-7.5.
        //
        // Workflows live in `<data_dir>/workflows/*.workflow`
        // (override via `RELIX_WORKFLOWS_DIR`). The chronicle
        // is a separate sqlite file in the same data dir so
        // workflow lifecycle doesn't entangle with the
        // coordinator's task-schema migrations.
        let workflows_dir = std::env::var("RELIX_WORKFLOWS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                coord_cfg
                    .db_path
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("workflows")
            });
        let workflow_store = crate::workflow::WorkflowStore::new(workflows_dir.clone());
        let workflow_chronicle_path = coord_cfg
            .db_path
            .parent()
            .map(crate::workflow::chronicle::default_chronicle_path)
            .unwrap_or_else(|| std::path::PathBuf::from("workflows.sqlite"));
        let workflow_chronicle = crate::workflow::WorkflowChronicle::open(&workflow_chronicle_path)
            .map_err(|e| format!("workflow chronicle open: {e}"))?;
        let workflow_dispatcher_cell: crate::workflow::WorkflowDispatcherCell =
            Arc::new(tokio::sync::OnceCell::new());
        let known_peers: std::collections::BTreeSet<String> = cfg.peers.keys().cloned().collect();
        crate::workflow::coordinator::register(
            bridge,
            workflow_store.clone(),
            workflow_chronicle.clone(),
            workflow_dispatcher_cell.clone(),
            Arc::new(known_peers),
        );
        let workflow_caps: &[(&str, &str, &[&str], bool)] = &[
            (
                "workflow.run",
                "Execute a workflow by name. Arg JSON: \
                 {\"name\": \"<workflow>\", \"input\": \"<text>\"}. \
                 Returns the full execution record.",
                &["workflow", "execute"],
                false,
            ),
            (
                "workflow.run.stream",
                "Streaming variant of workflow.run. Emits per-step JSON \
                 events (started / step_started / step_completed / \
                 step_failed / finished) as they happen — drives the \
                 bridge's POST /v1/workflows/run SSE response.",
                &["workflow", "execute", "stream"],
                true,
            ),
            (
                "workflow.list",
                "Enumerate every workflow file found in the \
                 workflows directory (returns name + description + version).",
                &["workflow", "read"],
                false,
            ),
            (
                "workflow.status",
                "Fetch a past execution by id. Arg JSON: \
                 {\"execution_id\": \"<hex>\"}.",
                &["workflow", "read"],
                false,
            ),
            (
                "workflow.validate",
                "Parse + validate a workflow source string. \
                 Arg JSON: {\"source\": \"<yaml>\"}. \
                 Returns {ok, error?} without touching the catalog.",
                &["workflow", "read"],
                false,
            ),
            (
                "workflow.reload",
                "Drop the workflow file cache. Operators call this \
                 after editing a .workflow file in place to pick up \
                 changes without a coordinator restart.",
                &["workflow", "mutate"],
                false,
            ),
        ];
        for (method, doc, cats, streaming) in workflow_caps {
            let mut desc = if *streaming {
                CapabilityDescriptor::stream_out(*method)
            } else {
                CapabilityDescriptor::unary(*method)
            };
            desc = desc.with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }
        // RELIX-7.24: planning.* coordinator capabilities.
        // The registry merges three sources (local manifest +
        // explicit [agents.<name>] capabilities + cached
        // peer manifests). The dispatcher_cell shared with
        // workflow.run lets planning.create_plan execute the
        // generated workflow when dry_run = false.
        let planning_registry = crate::planning::AgentCapabilityRegistry::from_sources(
            &cfg.controller.name,
            &manifest,
            &cfg.agents,
            // No peer-manifest cache wired at boot — the
            // bridge's ManifestCache populates async after
            // node.manifest round-trips. Operators get
            // declared-only agents until the cache warms.
            &std::collections::BTreeMap::new(),
        );
        let planning_cfg: crate::planning::PlanningConfig =
            cfg.planning.clone().unwrap_or_default();
        // RELIX-7.24 Stage-4: open the approval store when
        // require_approval is configured (or when the
        // operator supplied an explicit approval_db_path,
        // which signals intent to use the gate). When the
        // store opens, we also spawn the background expiry
        // sweep so pending plans don't pile up forever.
        let approval_store: Option<crate::planning::ApprovalStore> =
            if planning_cfg.require_approval || planning_cfg.approval_db_path.is_some() {
                let approval_path = planning_cfg.approval_db_path.clone().unwrap_or_else(|| {
                    coord_cfg
                        .db_path
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join("plan_approvals.sqlite")
                });
                match crate::planning::ApprovalStore::open(&approval_path) {
                    Ok(store) => {
                        tracing::info!(
                            path = %approval_path.display(),
                            timeout_secs = planning_cfg.approval_timeout_secs,
                            "planning: approval store opened — Stage-4 gate ENABLED"
                        );
                        let _sweep = crate::planning::spawn_approval_expiry_sweep(
                            store.clone(),
                            planning_cfg.approval_timeout_secs,
                        );
                        Some(store)
                    }
                    Err(e) => {
                        tracing::error!(
                            path = %approval_path.display(),
                            error = %e,
                            "planning: failed to open approval store — Stage-4 gate DISABLED for \
                             this boot; planning.create_plan with require_approval=true will \
                             return RESPONDER_INTERNAL"
                        );
                        None
                    }
                }
            } else {
                None
            };
        // PART 9: wire the dual-write decision mirror. When BOTH
        // the generic `ApprovalDeliveryService` and the planning
        // `ApprovalStore` are alive, a decision recorded on
        // either side flips the matching row in the other store
        // automatically. The pair shares the `id` column
        // (plan_id ↔ approval_id) so operators don't have to
        // teach two different systems to stay in sync.
        if let (Some(svc), Some(ps)) = (research_approval.as_ref(), approval_store.as_ref()) {
            crate::approval::wire_dual_write(svc, ps);
        }
        crate::planning::register(
            bridge,
            planning_registry.clone(),
            workflow_dispatcher_cell.clone(),
            planning_cfg,
            approval_store,
        );
        for (method, doc) in crate::planning::planning_capability_descriptors() {
            let cats: &[&str] = if *method == "planning.create_plan" {
                &["planning", "execute"]
            } else {
                &["planning", "read"]
            };
            manifest.add_capability(
                CapabilityDescriptor::unary(*method)
                    .with_description(*doc)
                    .with_categories(cats.iter().map(|s| (*s).into())),
            );
        }
        tracing::info!(
            agent_count = planning_registry.agent_count(),
            "planning: AgentCapabilityRegistry seeded + planning.* coordinator caps registered"
        );

        // Post-startup wiring: dial every peer + publish a
        // MeshWorkflowDispatcher into the cell. Done outside
        // this boot path so the rpc::Client can finish coming
        // up first.
        if !cfg.peers.is_empty() {
            out.push(StartupWiring::CoordWorkflowDispatcher {
                cell: workflow_dispatcher_cell,
                peers: cfg.peers.clone(),
                deadline_secs: coord_cfg
                    .ai_peer
                    .as_ref()
                    .map(|c| c.deadline_secs)
                    .unwrap_or(30),
            });
        } else {
            tracing::info!("coordinator: no [peers] configured; workflow dispatcher disabled");
        }
        tracing::info!(
            workflows_dir = %workflows_dir.display(),
            chronicle = %workflow_chronicle_path.display(),
            "coordinator node: registered workflow.run / list / status / validate"
        );

        // ── RELIX-7.7 / 7.11 GAP 2: channel routing. Build the
        // router from `[routing]` validated against `[peers]`,
        // register `routing.resolve` + `routing.list`. Absent
        // config means an empty router → channels fall back to
        // the static `("ai", "ai.chat")` target.
        let routing_rules = cfg
            .routing
            .as_ref()
            .map(|r| r.rules.clone())
            .unwrap_or_default();
        let known_peers: std::collections::BTreeSet<String> = cfg.peers.keys().cloned().collect();
        let router = if routing_rules.is_empty() {
            crate::nodes::coordinator::routing::ChannelRouter::empty()
        } else {
            crate::nodes::coordinator::routing::ChannelRouter::new(routing_rules, &known_peers)
                .map_err(|e| format!("[routing] validation: {e}"))?
        };
        crate::nodes::coordinator::routing::register(bridge, router.clone());
        for (method, doc) in [
            (
                "routing.resolve",
                "RELIX-7.7/7.11 GAP 2: resolve an inbound channel message to a target \
                 agent + capability. Args: JSON \
                 {channel, sender, subject?, content?}. Returns \
                 {decision: {target_agent, capability, matched_rule?}, rules_evaluated}.",
            ),
            (
                "routing.list",
                "Return every routing rule currently configured on this coordinator as \
                 JSON. Operator inspection surface.",
            ),
        ] {
            manifest.add_capability(
                CapabilityDescriptor::unary(method)
                    .with_description(doc)
                    .with_categories(["routing".into(), "read".into()]),
            );
        }
        tracing::info!(
            rules = router.rules().len(),
            "coordinator node: registered routing.resolve / routing.list"
        );

        // ── RELIX-7.11 metrics caps. Registered on the
        // coordinator so bridge /v1/metrics/* proxies have a
        // single peer to talk to. The metrics store + query
        // engine + alert engine live in the metrics bundle the
        // controller built at startup; absent bundle means
        // metrics caps stay unregistered and the bridge returns
        // 503 for the operator.
        if let Some(b) = metrics.as_ref() {
            let alert_engine = b.alert_engine.clone();
            crate::metrics::coordinator::register(
                bridge,
                b.query.clone(),
                Some(alert_engine.clone()),
            );
            for (method, doc) in metrics_capability_descriptors() {
                manifest.add_capability(
                    CapabilityDescriptor::unary(*method)
                        .with_description(*doc)
                        .with_categories(["metrics".into(), "read".into()]),
                );
            }
            // RELIX-7.28 Part 2 — observability dashboard capabilities.
            let alert_engine_arc = std::sync::Arc::new(alert_engine.clone());
            crate::metrics::observability::register(
                bridge,
                b.query.clone(),
                b.alert_chronicle.clone(),
                alert_engine_arc.clone(),
                budget.as_ref().map(|b| b.enforcer.clone()),
            );
            for (method, doc) in
                crate::metrics::observability::observability_capability_descriptors()
            {
                manifest.add_capability(
                    CapabilityDescriptor::unary(*method)
                        .with_description(*doc)
                        .with_categories(["observability".into(), "read".into()]),
                );
            }
            // RELIX-7.11 GAP 3+4: compose the alert delivery
            // sinks. The chronicle sink ALWAYS runs so an
            // operator without configured channel targets
            // still has a persistent audit trail. The
            // multi-channel sink fans out to Telegram /
            // Discord / Slack / Email when
            // `[metrics.alerts.targets]` is configured. The
            // logging sink stays on for `tracing` consumers
            // (Loki / Grafana log scrape, etc.).
            let chronicle_sink: std::sync::Arc<dyn crate::metrics::alert::AlertDeliver> =
                std::sync::Arc::new(crate::metrics::ChronicleAlertSink::new(
                    b.alert_chronicle.clone(),
                ));
            let channel_sink: std::sync::Arc<dyn crate::metrics::alert::AlertDeliver> =
                std::sync::Arc::new(crate::metrics::MultiChannelAlertSink::new(
                    b.alert_mesh_cell.clone(),
                    b.alert_targets.clone(),
                ));
            let logging_sink: std::sync::Arc<dyn crate::metrics::alert::AlertDeliver> =
                std::sync::Arc::new(crate::metrics::alert::LoggingAlertSink);
            let composite = crate::metrics::CompositeAlertSink::new(vec![
                chronicle_sink,
                channel_sink,
                logging_sink,
            ]);
            // Wire the channel-fan-out's mesh client post-
            // startup once the rpc::Client is up.
            out.push(StartupWiring::CoordAlertMesh {
                cell: b.alert_mesh_cell.clone(),
                peers: cfg.peers.clone(),
                deadline_secs: 30,
            });
            // Spawn the alert-engine evaluation loop. Drops
            // the JoinHandle — the loop runs for the lifetime
            // of the controller.
            let interval = std::time::Duration::from_secs(b.alert_interval_secs.max(5));
            let _alert_handle =
                alert_engine.spawn(interval, crate::metrics::alert::AlertSink::new(composite));
            tracing::info!(
                interval_secs = b.alert_interval_secs,
                alert_targets = b.alert_targets.len(),
                "coordinator node: registered metrics.* capabilities + spawned alert engine + chronicle"
            );

            // ── GAP 22 Feature 2 follow-up: cost-baseline store +
            // spike detector. Wired when `[metrics.cost_alerts]
            // enabled = true`. The detector runs on its own tick
            // alongside the existing AlertEngine; the two paths
            // are complementary (engine fires live; detector
            // archives + checks against the persistent baseline).
            if b.cost_alerts_cfg.enabled {
                let path = b.cost_alerts_cfg.db_path.clone().unwrap_or_else(|| {
                    let mut p = b.metrics_db_path.clone();
                    p.set_file_name("cost_baselines.db");
                    p
                });
                match crate::metrics::cost_baseline::CostBaselineStore::open(&path) {
                    Ok(baseline_store) => {
                        crate::metrics::coordinator::register_baseline_caps(
                            bridge,
                            baseline_store.clone(),
                        );
                        for (method, doc) in [
                            (
                                "metrics.cost_baselines",
                                "List persisted per-model cost baseline windows.",
                            ),
                            (
                                "metrics.ask_human_baselines",
                                "List persisted per-agent ask-human-rate baseline windows.",
                            ),
                            (
                                "metrics.cost_spike_history",
                                "List the most recent archived cost-spike fire events.",
                            ),
                        ] {
                            manifest.add_capability(
                                CapabilityDescriptor::unary(method)
                                    .with_description(doc)
                                    .with_categories([
                                        "read".into(),
                                        "metrics".into(),
                                        "observability".into(),
                                    ]),
                            );
                        }
                        let detector_sink = crate::metrics::alert::AlertSink::new(
                            crate::metrics::alert::LoggingAlertSink,
                        );
                        let detector = crate::metrics::spike_detector::CostSpikeDetector::new(
                            b.cost_alerts_cfg.clone(),
                            b.query.clone(),
                            baseline_store,
                            detector_sink,
                        );
                        let _detector_handle = detector.spawn();
                        tracing::info!(
                            path = %path.display(),
                            "coordinator node: GAP 22 Feature 2 spike detector online"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "coordinator: cost baseline store open failed; spike detector NOT spawned"
                        );
                    }
                }
            }
        } else {
            tracing::info!(
                "coordinator: [metrics] disabled — metrics.* capabilities not registered"
            );
        }

        // ── RELIX-7.28 Part 1: budget.* capabilities.
        if let Some(b) = budget.as_ref() {
            crate::metrics::budget_coordinator::register(bridge, b.enforcer.clone());
            for (method, doc) in crate::metrics::budget_coordinator::budget_capability_descriptors()
            {
                manifest.add_capability(
                    CapabilityDescriptor::unary(*method)
                        .with_description(*doc)
                        .with_categories(["budget".into(), "observability".into()]),
                );
            }
            tracing::info!(
                "coordinator node: registered budget.status / budget.reset capabilities"
            );
        }

        // ── RELIX-7.28 Part 3: pii.* capabilities.
        if let Some(gate) = pii_gate.as_ref() {
            crate::nodes::pii_gate_coordinator::register(bridge, (*gate).clone());
            for (method, doc) in crate::nodes::pii_gate_coordinator::pii_capability_descriptors() {
                manifest.add_capability(
                    CapabilityDescriptor::unary(*method)
                        .with_description(*doc)
                        .with_categories(["pii".into(), "observability".into(), "read".into()]),
                );
            }
            tracing::info!(
                action = %gate.action().as_str(),
                "coordinator node: registered pii.scan_stats / pii.recent_events capabilities"
            );
        }

        // ── RELIX-7.15: training data pipeline. Six unary
        // capabilities on the coordinator backed by the
        // bundle's TrainingStore + ExportEngine. The bundle
        // already spawned the drain / retention / scorer
        // loops; here we just register the dispatch handlers.
        if let Some(b) = training.as_ref() {
            crate::training::register(
                bridge,
                b.store.clone(),
                b.export_dir.clone(),
                b.anonymizer.clone(),
            );
            for (method, doc) in crate::training::training_capability_descriptors() {
                let categories: &[&str] = match *method {
                    "training.list_interactions" => &["training", "read"],
                    "training.get_interaction" => &["training", "read"],
                    "training.export" => &["training", "export", "mutate"],
                    "training.score_interaction" => &["training", "mutate"],
                    "training.stats" => &["training", "read"],
                    "training.delete_interaction" => &["training", "mutate"],
                    "training.pii_scan" => &["training", "pii", "read"],
                    "training.anonymize_preview" => &["training", "pii", "read"],
                    _ => &["training"],
                };
                manifest.add_capability(
                    CapabilityDescriptor::unary(*method)
                        .with_description(*doc)
                        .with_categories(categories.iter().map(|s| (*s).into())),
                );
            }
            tracing::info!(
                export_dir = %b.export_dir.display(),
                "coordinator node: registered training.* capabilities"
            );
        } else {
            tracing::info!(
                "coordinator: [training] disabled — training.* capabilities not registered"
            );
        }

        // ── Delegation — optional [coordinator.delegation] section.
        let delegation_cfg_value = cfg
            .coordinator
            .as_ref()
            .and_then(|v| v.get("delegation").cloned());
        let delegation_cfg: crate::nodes::coordinator::delegate::DelegationConfig =
            match delegation_cfg_value {
                Some(raw) => raw
                    .try_into()
                    .map_err(|e: toml::de::Error| format!("[coordinator.delegation] parse: {e}"))?,
                None => crate::nodes::coordinator::delegate::DelegationConfig::default(),
            };
        crate::nodes::coordinator::delegate::register(
            bridge,
            store.clone(),
            delegation_cfg.max_depth,
        );
        let delegate_caps: &[(&str, &str, &[&str])] = &[
            (
                "delegate.spawn",
                "Spawn a delegated child task. Arg: \
                 parent_task_id|goal|context|target_subject_id|depth. \
                 Enforces a configurable max delegation depth (default 3).",
                &["delegate", "task", "persist"],
            ),
            (
                "delegate.result",
                "Read a delegated child's status + result preview + \
                 completed_at (sentinel -1 when not terminal).",
                &["delegate", "task", "read"],
            ),
            (
                "delegate.cancel",
                "Cancel a delegated child task. Refuses when the task is \
                 already in a terminal state.",
                &["delegate", "task", "mutate"],
            ),
            (
                "delegate.list",
                "List delegated children of a parent task. Returns rows \
                 `child_task_id\\tgoal_preview\\tstatus\\tcreated_at` \
                 plus a trailing `count=N` line.",
                &["delegate", "task", "read"],
            ),
        ];
        for (method, doc, cats) in delegate_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }

        let delegation_ai_cell: crate::nodes::coordinator::delegate::DelegationAiDispatcherCell =
            Arc::new(tokio::sync::OnceCell::new());
        if cfg
            .coordinator
            .as_ref()
            .is_some_and(|v| v.get("delegation").is_some())
            && delegation_cfg.enabled
        {
            crate::nodes::coordinator::delegate::spawn_delegation_executor(
                store.clone(),
                delegation_ai_cell.clone(),
                delegation_cfg.clone(),
            );
            tracing::info!(
                max_depth = delegation_cfg.max_depth,
                max_concurrent = delegation_cfg.max_concurrent,
                executor_poll_secs = delegation_cfg.executor_poll_secs,
                "coordinator node: delegation executor spawned"
            );
            if let Some(ai_peer) = delegation_cfg.ai_peer.clone() {
                let key_path = cfg.identity.key_path.clone();
                let cell = delegation_ai_cell.clone();
                tokio::spawn(async move {
                    populate_delegation_ai_cell(cell, ai_peer, key_path).await;
                });
            } else {
                tracing::info!(
                    "coordinator: no [coordinator.delegation.ai_peer]; \
                     delegation AI dispatch disabled"
                );
            }
        } else {
            tracing::info!(
                "coordinator: delegation executor not enabled \
                 ([coordinator.delegation] missing or enabled=false)"
            );
        }
        tracing::info!(
            "coordinator node: registered delegate.spawn / result / cancel / list \
             (max_depth={})",
            delegation_cfg.max_depth
        );

        // ── Agent employee permission model ────────────────
        // Stored alongside the existing task ledger. Always
        // opened — capabilities are always live so SOL flows
        // can manage agents even when the gate-side wiring
        // (set_agent_gate) is deferred.
        // Provision the operator-console agent profile so the
        // dashboard/bridge identity passes the fail-closed agent gate
        // (Tasks/Workflows). The subject id is supplied by the boot
        // path via RELIX_OPERATOR_CONSOLE_SUBJECT (the bridge AIC's
        // subject). This does NOT weaken the gate — it provisions a
        // real, audited profile; absent the env var nothing is seeded.
        if let Ok(op_subject) = std::env::var("RELIX_OPERATOR_CONSOLE_SUBJECT") {
            let op_subject = op_subject.trim().to_string();
            if !op_subject.is_empty() {
                match agent_store.ensure_operator_console_profile(&op_subject, "default") {
                    Ok(true) => tracing::info!(
                        subject_id = %op_subject,
                        "coordinator: provisioned operator-console agent profile (allow-all)"
                    ),
                    Ok(false) => tracing::debug!(
                        subject_id = %op_subject,
                        "coordinator: operator-console agent profile already present"
                    ),
                    Err(e) => tracing::warn!(
                        subject_id = %op_subject,
                        error = %e,
                        "coordinator: failed to provision operator-console agent profile"
                    ),
                }
                // First-run owner authority: grant the console identity
                // the full Org/Work Keys so the dashboard owner can stand
                // up + assign the first team (idempotent, self-healing).
                match agent_store.grant_console_authority(&op_subject, "default") {
                    Ok(true) => tracing::info!(
                        subject_id = %op_subject,
                        "coordinator: granted operator-console owner authority (assign/manage/spawn/configure)"
                    ),
                    Ok(false) => tracing::debug!(
                        subject_id = %op_subject,
                        "coordinator: operator-console owner authority already present or no console profile"
                    ),
                    Err(e) => tracing::warn!(
                        subject_id = %op_subject,
                        error = %e,
                        "coordinator: failed to grant operator-console owner authority"
                    ),
                }
            }
        }
        // Shared Rig registry (builtins + CLI subscription Rigs).
        // Available regardless of the heartbeat loop so `rig.list`
        // can tell the Keys / agent-config UI which backends exist.
        let rig_registry = std::sync::Arc::new({
            let mut r = crate::rig::RigRegistry::with_builtins();
            crate::rig::register_cli_rigs(&mut r);
            // Optional Guild-default Rig so an Operative with no Rig
            // of its own still dispatches (opt-in via env).
            if let Ok(d) = std::env::var("RELIX_DEFAULT_RIG") {
                let d = d.trim();
                if !d.is_empty() {
                    r.set_default(Some(d.to_string()));
                }
            }
            r
        });
        {
            let reg = rig_registry.clone();
            bridge.register(
                "rig.list",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |_ctx: crate::dispatch::InvocationCtx| {
                        let reg = reg.clone();
                        async move {
                            crate::dispatch::HandlerOutcome::Ok(reg.names().join("\n").into_bytes())
                        }
                    },
                )),
            );
        }
        {
            // Structured Rig feed (name + label + governance) for the
            // agent-config UI to render backend choices.
            let reg = rig_registry.clone();
            bridge.register(
                "rig.describe",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |_ctx: crate::dispatch::InvocationCtx| {
                        let reg = reg.clone();
                        async move {
                            match serde_json::to_vec(&reg.describe()) {
                                Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("rig.describe encode: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `brief.run` — synchronous, on-demand execution of one Brief
            // through its Operative's Rig (the dashboard "Start" action).
            // Works regardless of the heartbeat loop; returns a structured
            // RunReport (real outcome OR a clear adapter-unavailable
            // refusal, never a faked run).
            let reg = rig_registry.clone();
            let st = store.clone();
            let ags = agent_store.clone();
            bridge.register(
                "brief.run",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let reg = reg.clone();
                        let st = st.clone();
                        let ags = ags.clone();
                        async move {
                            // Args are `brief_id` or `brief_id|rig_override`.
                            // A non-empty rig override forces that Rig (e.g.
                            // `echo` for the golden-path smoke) instead of the
                            // assignee's configured Rig; an unknown Rig refuses
                            // cleanly via the normal `no_adapter` path.
                            let raw = String::from_utf8_lossy(&ctx.args);
                            let mut parts = raw.splitn(2, '|');
                            let brief_id = parts.next().unwrap_or("").trim().to_string();
                            let rig_override = parts
                                .next()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_string());
                            if brief_id.is_empty() {
                                return crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: "brief.run: brief_id required".into(),
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                );
                            }
                            // Resolve the assignee's preferred Rig + charter,
                            // and compose the prompt the Rig will execute.
                            // `assignee` is also captured so a refused attempt
                            // can be attributed to the Operative (when one is
                            // set).
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let (preferred, charter, assignee, prefs) = match st.brief_card(&brief_id) {
                                Ok(Some(card)) => {
                                    let assignee =
                                        card.assignee_agent_id.clone().unwrap_or_default();
                                    let agent = card
                                        .assignee_agent_id
                                        .as_deref()
                                        .and_then(|a| ags.get_agent_for_tenant(a, &tenant).ok().flatten());
                                    // The Operative's stored model/effort hints —
                                    // a supported CLI Rig maps them to its
                                    // `--model` / `-c model_reasoning_effort` flags.
                                    let prefs = agent
                                        .as_ref()
                                        .map(|a| {
                                            crate::nodes::coordinator::heartbeat::RunModelPrefs::new(
                                                a.model_preference.clone(),
                                                a.reasoning_effort.clone(),
                                            )
                                        })
                                        .unwrap_or_default();
                                    (
                                        agent.as_ref().and_then(|a| a.rig.clone()),
                                        agent
                                            .map(|a| a.instruction_bundle)
                                            .filter(|c| !c.trim().is_empty()),
                                        assignee,
                                        prefs,
                                    )
                                }
                                _ => (
                                    None,
                                    None,
                                    String::new(),
                                    crate::nodes::coordinator::heartbeat::RunModelPrefs::default(),
                                ),
                            };
                            let prompt = st
                                .compose_brief_prompt_with_charter(&brief_id, 10, charter.as_deref());
                            let internal = |cause: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause,
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                )
                            };
                            // Async dispatch through the SHARED run chokepoint
                            // (`preflight_and_spawn`): pre-flight synchronously
                            // (resolve adapter, refuse clearly if unavailable,
                            // win the Claim, open the run record) — fast — then
                            // hand the blocking `rig.run` to a background thread
                            // so a long Claude/Codex Shift never freezes the
                            // bridge. Returns immediately with the run_id +
                            // status `running`; the dashboard polls `/v1/runs`.
                            // The SAME helper backs Prime's Start-to-Shift.
                            let bridge_tokens = crate::rig::bridge::BridgeTokenStore::global();
                            // An explicit override wins over the assignee's Rig.
                            let chosen_rig = rig_override.as_deref().or(preferred.as_deref());
                            let report =
                                match crate::nodes::coordinator::heartbeat::preflight_and_spawn(
                                    &st,
                                    &reg,
                                    Some(&bridge_tokens),
                                    300,
                                    &brief_id,
                                    chosen_rig,
                                    prompt,
                                    prefs,
                                ) {
                                    Err(e) => return internal(format!("brief.run: {e}")),
                                    Ok(report) => {
                                        // A pre-run refusal (no Shift started)
                                        // is persisted as a durable `refused`
                                        // Shift so the Brief can later answer
                                        // "why didn't it run?". Tenant-gated +
                                        // a no-op for not_found / already_running.
                                        if report.run_id.is_none() {
                                            let _ = st.record_manual_refusal_for_tenant(
                                                &brief_id,
                                                &tenant,
                                                &assignee,
                                                &report.rig,
                                                &report.status,
                                                &report.summary,
                                            );
                                        }
                                        report
                                    }
                                };
                            match serde_json::to_vec(&report) {
                                Ok(body) => crate::dispatch::HandlerOutcome::Ok(body),
                                Err(e) => internal(format!("brief.run encode: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.retry` — STAGE-2 guarded operator retry of a source failed
            // Shift (`POST /v1/runs/:run_id/retry`, execution-and-issue §3.3b).
            // A one-click operator recovery action, NOT a blind auto-retry loop:
            // the runtime REFUSES unless the source is terminal-and-failure-like,
            // retryable, has budget, links a still-present in-tenant Brief, and
            // has no existing retry child. Eligible ⇒ it opens exactly ONE child
            // run through the SAME preflight/execute path as `brief.run` (shared
            // adapter resolution, Claim, workspace prep, ledger, governance) and
            // links it back to the source. Arg: the source run id.
            let reg = rig_registry.clone();
            let st = store.clone();
            let ags = agent_store.clone();
            bridge.register(
                "run.retry",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let reg = reg.clone();
                        let st = st.clone();
                        let ags = ags.clone();
                        async move {
                            let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let internal = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: c,
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                )
                            };
                            // Cross-Guild / unknown run id → a generic not-found
                            // (no existence leak). The bridge maps a "not found"
                            // cause onto 404.
                            let not_found = || {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: format!("run not found: {run_id}"),
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            if run_id.is_empty() {
                                return crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: "run.retry: run_id required".into(),
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                );
                            }
                            // Early tenant gate — refuse a cross-Guild/unknown run
                            // before any prompt work (defense in depth; the
                            // precheck inside `open_retry_child` re-gates).
                            match st.run_belongs_to_tenant(&run_id, &tenant) {
                                Ok(true) => {}
                                Ok(false) => return not_found(),
                                Err(e) => return internal(format!("run.retry: {e}")),
                            }
                            // Resolve the source's Brief + the assignee's Rig /
                            // charter, and compose the retry prompt — IDENTICAL to
                            // `brief.run` so a retry runs exactly like a fresh run.
                            let brief_id = match st.get_run(&run_id) {
                                Ok(Some(r)) => r.brief_id,
                                Ok(None) => return not_found(),
                                Err(e) => return internal(format!("run.retry: {e}")),
                            };
                            let (preferred, charter, prefs) = match st.brief_card(&brief_id) {
                                Ok(Some(card)) => {
                                    // Tenant-scoped Operative lookup (the caller has a
                                    // tenant) so the retry child inherits the SAME
                                    // Rig + charter + model/effort prefs the original
                                    // Shift would have used — no cross-Guild read.
                                    let agent = card
                                        .assignee_agent_id
                                        .as_deref()
                                        .and_then(|a| ags.get_agent_for_tenant(a, &tenant).ok().flatten());
                                    let prefs = agent
                                        .as_ref()
                                        .map(|a| {
                                            crate::nodes::coordinator::heartbeat::RunModelPrefs::new(
                                                a.model_preference.clone(),
                                                a.reasoning_effort.clone(),
                                            )
                                        })
                                        .unwrap_or_default();
                                    (
                                        agent.as_ref().and_then(|a| a.rig.clone()),
                                        agent
                                            .map(|a| a.instruction_bundle)
                                            .filter(|c| !c.trim().is_empty()),
                                        prefs,
                                    )
                                }
                                _ => (
                                    None,
                                    None,
                                    crate::nodes::coordinator::heartbeat::RunModelPrefs::default(),
                                ),
                            };
                            let prompt = st.compose_brief_prompt_with_charter(
                                &brief_id,
                                10,
                                charter.as_deref(),
                            );
                            let bridge_tokens = crate::rig::bridge::BridgeTokenStore::global();
                            match crate::nodes::coordinator::heartbeat::open_retry_child(
                                &st,
                                &reg,
                                Some(&bridge_tokens),
                                300,
                                &run_id,
                                &tenant,
                                prompt,
                                preferred.as_deref(),
                                prefs,
                            ) {
                                Err(e) => internal(format!("run.retry: {e}")),
                                Ok(outcome) => {
                                    use crate::nodes::coordinator::heartbeat::RetryOpen;
                                    let body = match outcome {
                                        RetryOpen::NotFound => return not_found(),
                                        RetryOpen::AlreadyRetried { child_run_id } => {
                                            // Idempotent: return the EXISTING child
                                            // id (mapped to 200) — no second run.
                                            serde_json::json!({
                                                "status": "already_retried",
                                                "run_id": child_run_id,
                                                "retried_from_run_id": run_id.clone(),
                                                "message": "this Shift was already retried — returning the existing child run",
                                            })
                                        }
                                        RetryOpen::Refused(report) => {
                                            // Honest refusal — `status` drives the
                                            // bridge status code; `error` surfaces
                                            // the reason to the dashboard.
                                            serde_json::json!({
                                                "status": report.status,
                                                "error": report.summary,
                                                "reason": report.summary,
                                                "brief_id": report.brief_id,
                                                "rig": report.rig,
                                            })
                                        }
                                        RetryOpen::Ready {
                                            ready,
                                            source_run_id,
                                            child_run_id,
                                            attempt,
                                        } => {
                                            // Committed: report `running` now + run
                                            // the blocking adapter on a background
                                            // thread (never freeze the bridge).
                                            let rig = ready.rig_name.clone();
                                            let workspace = ready.workspace.clone();
                                            let st_bg = st.clone();
                                            tokio::task::spawn_blocking(move || {
                                                let bt =
                                                    crate::rig::bridge::BridgeTokenStore::global();
                                                let _ = crate::nodes::coordinator::heartbeat::execute_ready(
                                                    &st_bg,
                                                    Some(&bt),
                                                    *ready,
                                                );
                                            });
                                            serde_json::json!({
                                                "status": "running",
                                                "run_id": child_run_id,
                                                "retried_from_run_id": source_run_id,
                                                "retry_attempt": attempt,
                                                "brief_id": brief_id,
                                                "rig": rig,
                                                "workspace": workspace,
                                                "summary": "retry started",
                                            })
                                        }
                                    };
                                    match serde_json::to_vec(&body) {
                                        Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                        Err(e) => internal(format!("run.retry encode: {e}")),
                                    }
                                }
                            }
                        }
                    },
                )),
            );
        }
        if let Some(spine) = spine_store_for_agent_caps.clone() {
            // `prime.start` — Start-to-Shift (company-model §12.5B). Turns an
            // APPROVED Prime proposal's READY Briefs into real Shifts through
            // the SAME run chokepoint as `brief.run` (preflight_and_spawn). It
            // creates no Mandate/Brief/hire and changes no budget — it only
            // RUNS Briefs that are already assigned, active, and unblocked;
            // every skipped Brief is returned with an honest reason. Registered
            // only when the spine store opened (needs the approved-proposal
            // record). Arg: proposal_id (optionally `proposal_id|max`).
            let reg = rig_registry.clone();
            let st = store.clone();
            let ags = agent_store.clone();
            bridge.register(
                "prime.start",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let reg = reg.clone();
                        let st = st.clone();
                        let ags = ags.clone();
                        let spine = spine.clone();
                        async move {
                            crate::nodes::coordinator::agent::handlers::handle_prime_start(
                                &ags, &spine, &st, &reg, &ctx,
                            )
                        }
                    },
                )),
            );
        }
        if let Some(spine) = spine_store_for_agent_caps.clone() {
            // `prime.autonomy_tick_now` — operator-triggered single bounded
            // autonomous Prime tick for the caller's Guild (Manual Autonomy Tick
            // v1; company-model §5.4/§8.2 — the Action Center's "next governed
            // step", here as an explicit wake-up of the same timer-driven
            // driver). It runs EXACTLY ONE `autonomous_prime_tick` scoped to the
            // caller's OWN Guild (never all Guilds) and returns the
            // PrimeAutonomyRecord list, so the operator can wake the loop once
            // and see what it considered/advanced/started. Still governed
            // autonomy: same standing-authority + budget + Rig + per-tick-max
            // gates the timer path uses; does NOT require the runtime switch ON,
            // but grants no new authority. Role-gated to operator/admin (worker →
            // POLICY_DENIED). Needs the agent + spine + task stores, the Rig
            // registry, and the metrics query (for the autonomous budget gate).
            let reg = rig_registry.clone();
            let st = store.clone();
            let ags = agent_store.clone();
            let mq = metrics.map(|m| m.query.clone());
            // Prime Deliberation v1 — manual-tick live path. The manual tick now
            // reuses the SAME mesh AI decider the background timer builds, so
            // `Run Prime now` exercises the live deliberation layer instead of
            // always reporting `unavailable`. No provider key enters the
            // coordinator: the decider performs the existing governed `ai.chat`
            // mesh call through the coordinator's EXISTING outbound mesh client
            // (the populated alert mesh cell). When the cell is unpopulated or the
            // switch is off the tick falls back deterministically, exactly as
            // before. Because `MeshAiDecider::deliberate` does a `Handle::block_on`,
            // the tick is run from a `spawn_blocking` thread (never the async
            // worker) — calling `block_on` on a runtime thread would panic.
            let prime_alert_cell = metrics.map(|m| m.alert_mesh_cell.clone());
            let prime_ai_peer = crate::nodes::coordinator::agent::prime_driver::prime_ai_peer();
            let prime_llm_session =
                crate::nodes::coordinator::agent::prime_driver::prime_llm_session();
            bridge.register(
                "prime.autonomy_tick_now",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let reg = reg.clone();
                        let st = st.clone();
                        let ags = ags.clone();
                        let spine = spine.clone();
                        let mq = mq.clone();
                        let prime_alert_cell = prime_alert_cell.clone();
                        let prime_ai_peer = prime_ai_peer.clone();
                        let prime_llm_session = prime_llm_session.clone();
                        async move {
                            use crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider;
                            use crate::nodes::coordinator::agent::prime_driver::{
                                MeshAiDecider, handle_prime_autonomy_tick_now_with_ai,
                                parse_prime_llm_deliberation, parse_prime_llm_orchestration,
                                parse_prime_llm_plan_package, parse_prime_llm_prioritization,
                                parse_prime_llm_strategy_draft, parse_prime_plan_package_trigger,
                            };
                            // Re-read all three Prime LLM switches (like the timer) so
                            // an operator can flip them without a restart: deliberation
                            // (action choice), strategy draft (proposed strategy body
                            // authoring), and prioritization (queue order among legal
                            // candidates). All opt-in, all fall back deterministically;
                            // none approves a gate.
                            let llm_enabled = parse_prime_llm_deliberation(
                                std::env::var("RELIX_PRIME_LLM_DELIBERATION")
                                    .ok()
                                    .as_deref(),
                            );
                            let strategy_llm_enabled = parse_prime_llm_strategy_draft(
                                std::env::var("RELIX_PRIME_LLM_STRATEGY_DRAFT")
                                    .ok()
                                    .as_deref(),
                            );
                            let prioritization_enabled = parse_prime_llm_prioritization(
                                std::env::var("RELIX_PRIME_LLM_PRIORITIZATION")
                                    .ok()
                                    .as_deref(),
                            );
                            let orchestration_llm_enabled = parse_prime_llm_orchestration(
                                std::env::var("RELIX_PRIME_LLM_ORCHESTRATION")
                                    .ok()
                                    .as_deref(),
                            );
                            let plan_package_llm_enabled = parse_prime_llm_plan_package(
                                std::env::var("RELIX_PRIME_LLM_PLAN_PACKAGE")
                                    .ok()
                                    .as_deref(),
                            );
                            // WHEN plan-package authoring fires (tail gap-fill vs
                            // active before-execute preemption); re-read each tick so
                            // an operator can flip it without a restart. Inert unless
                            // the master `RELIX_PRIME_LLM_PLAN_PACKAGE` switch is on.
                            let plan_package_trigger = parse_prime_plan_package_trigger(
                                std::env::var("RELIX_PRIME_PLAN_PACKAGE_TRIGGER")
                                    .ok()
                                    .as_deref(),
                            );
                            // Capture the runtime handle on the async thread; the
                            // blocking tick bridges the async mesh call through it.
                            let prime_handle = tokio::runtime::Handle::current();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let outcome = tokio::task::spawn_blocking(move || {
                                // Build the live decider when ANY switch is on AND the
                                // coordinator mesh cell is populated (a reachable
                                // outbound client). The SAME decider serves the
                                // deliberation pre-pass, the strategy-body author, and
                                // the prioritization ordering. Otherwise pass None →
                                // honest `unavailable`/`deterministic_only` fallback.
                                let prime_decider = if llm_enabled
                                    || strategy_llm_enabled
                                    || prioritization_enabled
                                    || orchestration_llm_enabled
                                    || plan_package_llm_enabled
                                {
                                    prime_alert_cell
                                        .as_ref()
                                        .and_then(|c| c.get())
                                        .map(|mc| {
                                            MeshAiDecider::new(
                                                prime_handle.clone(),
                                                mc.mesh.clone(),
                                                mc.identity.clone(),
                                                prime_ai_peer.clone(),
                                                prime_llm_session.clone(),
                                                30,
                                                Some(tenant.clone()),
                                            )
                                        })
                                } else {
                                    None
                                };
                                let prime_ai: Option<&dyn PrimeAiDecider> =
                                    prime_decider.as_ref().map(|d| d as &dyn PrimeAiDecider);
                                handle_prime_autonomy_tick_now_with_ai(
                                    &ags,
                                    &spine,
                                    &st,
                                    &reg,
                                    mq.as_ref(),
                                    &ctx,
                                    prime_ai,
                                    llm_enabled,
                                    strategy_llm_enabled,
                                    prioritization_enabled,
                                    orchestration_llm_enabled,
                                    plan_package_llm_enabled,
                                    plan_package_trigger,
                                )
                            })
                            .await;
                            match outcome {
                                Ok(o) => o,
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("prime.autonomy_tick_now join error: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `brief.runs` — the durable run ledger (`brief_runs`). With a
            // non-empty arg, the runs for that one Brief (the Shift history
            // on the Brief card); empty arg → the recent runs across all
            // Briefs (the Active Runs feed). Stable structured records —
            // run_id / brief_id / agent_id / rig / status / started_at /
            // finished_at / duration_secs / summary — so the dashboard no
            // longer parses event strings.
            let st = store.clone();
            bridge.register(
                "brief.runs",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let brief_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let result = if brief_id.is_empty() {
                                st.list_runs_for_tenant(&tenant, 100)
                            } else {
                                match st.task_tenant(&brief_id) {
                                    Ok(Some(t)) if t == tenant => st.runs_for_brief(&brief_id, 100),
                                    Ok(None) if tenant == "default" => {
                                        st.runs_for_brief(&brief_id, 100)
                                    }
                                    Ok(_) => {
                                        Err(crate::nodes::coordinator::CoordinatorError::NotFound(
                                            brief_id.clone(),
                                        ))
                                    }
                                    Err(e) => Err(e),
                                }
                            };
                            match result {
                                Ok(runs) => match serde_json::to_vec(&runs) {
                                    Ok(body) => crate::dispatch::HandlerOutcome::Ok(body),
                                    Err(e) => crate::dispatch::HandlerOutcome::Err(
                                        relix_core::types::ErrorEnvelope {
                                            kind:
                                                relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                            cause: format!("brief.runs encode: {e}"),
                                            retry_hint: 1,
                                            retry_after: None,
                                        },
                                    ),
                                },
                                Err(crate::nodes::coordinator::CoordinatorError::NotFound(id)) => {
                                    crate::dispatch::HandlerOutcome::Err(
                                        relix_core::types::ErrorEnvelope {
                                            kind: relix_core::types::error_kinds::INVALID_ARGS,
                                            cause: format!("brief.runs: not found: {id}"),
                                            retry_hint: 0,
                                            retry_after: None,
                                        },
                                    )
                                }
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("brief.runs: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.workspace_config` — the resolved run-workspace context
            // config (mode / project root / caps) so the dashboard Settings
            // can show how runs are sandboxed. No args. The project root is
            // operator config (a directory path), not a secret.
            let st = store.clone();
            bridge.register(
                "run.workspace_config",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |_ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let cfg = st.run_workspace_config();
                            // Runtime-mode flags the dashboard surfaces so an
                            // operator can see HOW runs execute: the unsafe
                            // `inherit` opt-out + whether the autonomous
                            // heartbeat loop is on. Read from the same env the
                            // coordinator uses (stable deployment config).
                            let inherit = std::env::var("RELIX_RUN_WORKSPACE_MODE")
                                .map(|v| v.trim().eq_ignore_ascii_case("inherit"))
                                .unwrap_or(false);
                            let heartbeat_enabled = std::env::var("RELIX_HEARTBEAT_ENABLED")
                                .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
                                .unwrap_or(false);
                            let heartbeat_interval_secs = std::env::var(
                                "RELIX_HEARTBEAT_INTERVAL_SECS",
                            )
                            .ok()
                            .and_then(|v| v.trim().parse::<u64>().ok())
                            .filter(|n| *n >= 1)
                            .unwrap_or(10);
                            // Stage-2 opt-in autonomous retry lane
                            // (execution-and-issue §3.3b). Default OFF + bounded;
                            // surfaced so Settings can show it like the heartbeat.
                            let autonomous_recovery_enabled =
                                crate::nodes::coordinator::heartbeat::parse_autonomous_recovery_enabled(
                                    std::env::var("RELIX_AUTONOMOUS_RECOVERY").ok().as_deref(),
                                );
                            let autonomous_recovery_max =
                                crate::nodes::coordinator::heartbeat::parse_autonomous_recovery_max(
                                    std::env::var("RELIX_AUTONOMOUS_RECOVERY_MAX").ok().as_deref(),
                                );
                            // Opt-in autonomous Prime driver (company-model
                            // §5.4/§8.2/§12.5B). Default OFF + bounded; surfaced
                            // so Settings shows it alongside the heartbeat + the
                            // autonomous recovery lane.
                            let autonomous_prime_enabled =
                                crate::nodes::coordinator::heartbeat::parse_autonomous_prime_enabled(
                                    std::env::var("RELIX_AUTONOMOUS_PRIME").ok().as_deref(),
                                );
                            let autonomous_prime_max =
                                crate::nodes::coordinator::heartbeat::parse_autonomous_prime_max(
                                    std::env::var("RELIX_AUTONOMOUS_PRIME_MAX").ok().as_deref(),
                                );
                            let autonomous_prime_interval_secs = std::env::var(
                                "RELIX_AUTONOMOUS_PRIME_INTERVAL_SECS",
                            )
                            .ok()
                            .and_then(|v| v.trim().parse::<u64>().ok())
                            .filter(|n| *n >= 1)
                            .unwrap_or(30);
                            let body = serde_json::json!({
                                "context": cfg.context.as_str(),
                                "project_root": cfg.project_root.to_string_lossy(),
                                "max_bytes": cfg.max_bytes,
                                "max_files": cfg.max_files,
                                "workspace_root": st.run_workspace_root().to_string_lossy(),
                                "inherit": inherit,
                                "heartbeat_enabled": heartbeat_enabled,
                                "heartbeat_interval_secs": heartbeat_interval_secs,
                                "autonomous_recovery_enabled": autonomous_recovery_enabled,
                                "autonomous_recovery_max": autonomous_recovery_max,
                                "autonomous_prime_enabled": autonomous_prime_enabled,
                                "autonomous_prime_max": autonomous_prime_max,
                                "autonomous_prime_interval_secs": autonomous_prime_interval_secs,
                            });
                            match serde_json::to_vec(&body) {
                                Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("run.workspace_config encode: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.get` — one run record by id (`GET /v1/runs/:id`).
            let st = store.clone();
            bridge.register(
                "run.get",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let internal = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: c,
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                )
                            };
                            match st.get_run(&run_id) {
                                Ok(Some(r)) => match serde_json::to_vec(&r) {
                                    Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                    Err(e) => internal(format!("run.get encode: {e}")),
                                },
                                Ok(None) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: format!("run not found: {run_id}"),
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                ),
                                Err(e) => internal(format!("run.get: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.events` — a run's transcript, chronological (`GET
            // /v1/runs/:id/events`). Bounded + already redacted.
            let st = store.clone();
            bridge.register(
                "run.events",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            match st.list_run_events(&run_id, 500) {
                                Ok(events) => match serde_json::to_vec(&events) {
                                    Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                    Err(e) => crate::dispatch::HandlerOutcome::Err(
                                        relix_core::types::ErrorEnvelope {
                                            kind:
                                                relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                            cause: format!("run.events encode: {e}"),
                                            retry_hint: 1,
                                            retry_after: None,
                                        },
                                    ),
                                },
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("run.events: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.cancel` — request cancellation of an in-flight run. Flips
            // the run's [`CancelRegistry`] flag; the `ProcessRig` wait loop
            // polls it and kills the child, then `execute_ready` records the
            // run `cancelled`. Truthful: `active` reflects whether a live
            // process was actually signalled (vs already-finished).
            let st = store.clone();
            bridge.register(
                "run.cancel",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let record = st.get_run(&run_id).ok().flatten();
                            let already_terminal = record
                                .as_ref()
                                .map(|r| r.status != "running")
                                .unwrap_or(false);
                            let active = crate::rig::CancelRegistry::global().request(&run_id);
                            let _ = st.append_run_event(
                                &run_id,
                                "cancel_requested",
                                "relix",
                                if active {
                                    "operator requested cancellation — killing the running process"
                                } else if already_terminal {
                                    "cancel requested but the run had already finished"
                                } else {
                                    "cancel requested but no live process is registered for this run"
                                },
                                None,
                                false,
                            );
                            // Surface the cancel request on the Brief's
                            // chronicle so the execution event stream
                            // (`run_cancel_requested`) sees it.
                            if let Some(rec) = record.as_ref() {
                                let _ = st.append_event(
                                    &rec.brief_id,
                                    "brief.run_cancel_requested",
                                    &format!("run {run_id}: cancel requested (active={active})"),
                                );
                            }
                            let body = serde_json::json!({
                                "run_id": run_id,
                                "requested": true,
                                "active": active,
                                "already_terminal": already_terminal,
                                "note": if active {
                                    "cancellation signalled; the run will report `cancelled`"
                                } else if already_terminal {
                                    "run already finished — nothing to cancel"
                                } else {
                                    "no live process registered (run may be on another node, or already done)"
                                },
                            });
                            match serde_json::to_vec(&body) {
                                Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("run.cancel encode: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.artifacts` — the changed files a run produced (`GET
            // /v1/runs/:id/artifacts`). Tenant-scoped: a run whose Brief is
            // in another Guild reads as not-found (no existence leak).
            let st = store.clone();
            bridge.register(
                "run.artifacts",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            match st.run_belongs_to_tenant(&run_id, &tenant) {
                                Ok(true) => {}
                                Ok(false) => return invalid(format!("run not found: {run_id}")),
                                Err(e) => return invalid(format!("run.artifacts: {e}")),
                            }
                            match st.list_run_artifacts(&run_id) {
                                Ok(a) => match serde_json::to_vec(&a) {
                                    Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                    Err(e) => invalid(format!("run.artifacts encode: {e}")),
                                },
                                Err(e) => invalid(format!("run.artifacts: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.artifact_preview` — a safe text preview of one artifact
            // (`GET /v1/runs/:id/artifacts/:aid/preview`). Arg
            // `run_id|artifact_id`. Tenant-scoped; binaries / large files /
            // missing files are refused; the path is server-resolved.
            let st = store.clone();
            bridge.register(
                "run.artifact_preview",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let arg = String::from_utf8_lossy(&ctx.args).to_string();
                            let mut parts = arg.splitn(2, '|');
                            let run_id = parts.next().unwrap_or("").trim().to_string();
                            let aid: i64 =
                                parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(-1);
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            match st.run_belongs_to_tenant(&run_id, &tenant) {
                                Ok(true) => {}
                                Ok(false) => return invalid(format!("run not found: {run_id}")),
                                Err(e) => return invalid(format!("run.artifact_preview: {e}")),
                            }
                            let art = match st.get_run_artifact(&run_id, aid) {
                                Ok(Some(a)) => a,
                                Ok(None) => return invalid(format!("artifact not found: {aid}")),
                                Err(e) => return invalid(format!("run.artifact_preview: {e}")),
                            };
                            use crate::nodes::coordinator::heartbeat::{
                                read_artifact_preview, PreviewOutcome,
                            };
                            let max = crate::nodes::coordinator::ARTIFACT_PREVIEW_MAX_BYTES;
                            let outcome = read_artifact_preview(
                                &art.workspace,
                                &art.rel_path,
                                art.is_text,
                                max,
                            );
                            let body = match outcome {
                                PreviewOutcome::Text { content, truncated } => serde_json::json!({
                                    "rel_path": art.rel_path, "kind": art.kind, "size": art.size,
                                    "is_text": true, "available": true, "truncated": truncated,
                                    "content": content,
                                }),
                                PreviewOutcome::Binary => serde_json::json!({
                                    "rel_path": art.rel_path, "kind": art.kind, "size": art.size,
                                    "is_text": false, "available": false,
                                    "reason": "binary or non-text file — no preview",
                                }),
                                PreviewOutcome::Missing => serde_json::json!({
                                    "rel_path": art.rel_path, "kind": art.kind, "size": art.size,
                                    "available": false,
                                    "reason": "file no longer exists in the workspace",
                                }),
                                PreviewOutcome::Unsafe => serde_json::json!({
                                    "rel_path": art.rel_path, "kind": art.kind,
                                    "available": false, "reason": "path refused (outside workspace)",
                                }),
                            };
                            match serde_json::to_vec(&body) {
                                Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                Err(e) => invalid(format!("run.artifact_preview encode: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.artifact_diff` — a SAFE, bounded unified diff for ONE
            // changed file of a run (`GET /v1/runs/:id/artifacts/:aid/diff`).
            // Arg `run_id|artifact_id`. Tenant-scoped (another Guild's run
            // reads as not-found). The "after" side is the run's workspace
            // output; the "before" side is the live project-root file ONLY
            // when it still matches the run's recorded baseline hash — else an
            // honest `available:false` ("diff unavailable; preview instead").
            // Binary / unsafe-path / moved-baseline never dump content.
            let st = store.clone();
            bridge.register(
                "run.artifact_diff",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let arg = String::from_utf8_lossy(&ctx.args).to_string();
                            let mut parts = arg.splitn(2, '|');
                            let run_id = parts.next().unwrap_or("").trim().to_string();
                            let aid: i64 = parts
                                .next()
                                .and_then(|s| s.trim().parse().ok())
                                .unwrap_or(-1);
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            match st.run_belongs_to_tenant(&run_id, &tenant) {
                                Ok(true) => {}
                                Ok(false) => return invalid(format!("run not found: {run_id}")),
                                Err(e) => return invalid(format!("run.artifact_diff: {e}")),
                            }
                            let art = match st.get_run_artifact(&run_id, aid) {
                                Ok(Some(a)) => a,
                                Ok(None) => return invalid(format!("artifact not found: {aid}")),
                                Err(e) => return invalid(format!("run.artifact_diff: {e}")),
                            };
                            use crate::nodes::coordinator::heartbeat::{
                                DiffOutcome, read_artifact_diff,
                            };
                            let root = st.run_workspace_config().project_root.clone();
                            let max = crate::nodes::coordinator::ARTIFACT_PREVIEW_MAX_BYTES;
                            let outcome = read_artifact_diff(
                                &art.workspace,
                                &root,
                                &art.rel_path,
                                &art.kind,
                                art.is_text,
                                art.baseline_hash.as_deref(),
                                max,
                            );
                            let body = match outcome {
                                DiffOutcome::Unified {
                                    diff,
                                    truncated,
                                    baseline,
                                } => serde_json::json!({
                                    "rel_path": art.rel_path, "kind": art.kind, "size": art.size,
                                    "available": true, "truncated": truncated,
                                    "baseline": baseline, "diff": diff,
                                }),
                                DiffOutcome::Unavailable { reason } => serde_json::json!({
                                    "rel_path": art.rel_path, "kind": art.kind, "size": art.size,
                                    "available": false, "reason": reason,
                                }),
                            };
                            match serde_json::to_vec(&body) {
                                Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                Err(e) => invalid(format!("run.artifact_diff encode: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.review` — record an operator accept/reject on a run
            // (`POST /v1/runs/:id/review`). Arg `run_id|decision|note`.
            // Tenant-scoped; only a `done` run is reviewable. Records a
            // `review` transcript event. Does NOT apply files (future).
            let st = store.clone();
            bridge.register(
                "run.review",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let arg = String::from_utf8_lossy(&ctx.args).to_string();
                            let mut parts = arg.splitn(3, '|');
                            let run_id = parts.next().unwrap_or("").trim().to_string();
                            let decision = parts.next().unwrap_or("").trim().to_string();
                            let note = parts.next().unwrap_or("").to_string();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            match st.run_belongs_to_tenant(&run_id, &tenant) {
                                Ok(true) => {}
                                Ok(false) => return invalid(format!("run not found: {run_id}")),
                                Err(e) => return invalid(format!("run.review: {e}")),
                            }
                            match st.set_run_review(&run_id, &decision, &note) {
                                Ok(state) => {
                                    let msg = if note.trim().is_empty() {
                                        format!("operator {state} the run")
                                    } else {
                                        format!("operator {state} the run — {}", note.trim())
                                    };
                                    let _ = st.append_run_event(
                                        &run_id, "review", "relix", &msg, None, false,
                                    );
                                    let body =
                                        serde_json::json!({"run_id": run_id, "review": state});
                                    match serde_json::to_vec(&body) {
                                        Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                        Err(e) => invalid(format!("run.review encode: {e}")),
                                    }
                                }
                                Err(e) => invalid(format!("run.review: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.diff` — the safe-apply PLAN for a run (`GET
            // /v1/runs/:id/diff`). PURE preview: never mutates files.
            // Reports per-file action / conflict and whether the run is
            // apply-eligible (done + accepted + scoped workspace).
            // Tenant-scoped: another Guild's run reads as not-found.
            let st = store.clone();
            bridge.register(
                "run.diff",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            match execute_run_diff(&st, &run_id, &tenant) {
                                Ok(body) => match serde_json::to_vec(&body) {
                                    Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                    Err(e) => invalid(format!("run.diff encode: {e}")),
                                },
                                Err(c) => invalid(c),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.apply` — apply an accepted run's changed files back into
            // the configured project root (`POST /v1/runs/:id/apply`).
            // Refuses the WHOLE apply if ANY file is unsafe / conflicted (no
            // partial apply, no `force`). Tenant-scoped; only a `done` +
            // `accepted` + scoped-workspace run applies. Records
            // apply.plan / apply.started / apply.applied / apply.conflicted
            // / apply.failed transcript events and a durable apply status.
            let st = store.clone();
            bridge.register(
                "run.apply",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            match execute_run_apply(&st, &run_id, &tenant) {
                                Ok(body) => match serde_json::to_vec(&body) {
                                    Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                    Err(e) => invalid(format!("run.apply encode: {e}")),
                                },
                                Err(c) => invalid(c),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `run.discard` — discard a terminal run's output
            // (`POST /v1/runs/:id/discard`). Marks the run `discarded` (and
            // rejects a `done` run's review so it can never be applied),
            // records a `discarded` transcript event + a `brief.run_discarded`
            // Chronicle note, and leaves the scoped workspace for the normal
            // storage prune (no immediate delete). Tenant-scoped; a running
            // run is refused (cancel it first).
            let st = store.clone();
            bridge.register(
                "run.discard",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            match st.run_belongs_to_tenant(&run_id, &tenant) {
                                Ok(true) => {}
                                Ok(false) => return invalid(format!("run not found: {run_id}")),
                                Err(e) => return invalid(format!("run.discard: {e}")),
                            }
                            match st.discard_run(&run_id) {
                                Ok(state) => {
                                    let body = serde_json::json!({
                                        "run_id": run_id, "apply_status": state,
                                    });
                                    match serde_json::to_vec(&body) {
                                        Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                        Err(e) => invalid(format!("run.discard encode: {e}")),
                                    }
                                }
                                Err(e) => invalid(format!("run.discard: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `rig.runtime_state.get` — the persisted adapter runtime state
            // for one agent (`GET /v1/runs/runtime-state?agent_id=...`).
            // Arg is the agent id (raw string). Tenant-scoped: only the
            // caller's Guild rows are returned. Returns a JSON array, newest
            // first (empty if no run has populated state yet).
            let st = store.clone();
            bridge.register(
                "rig.runtime_state.get",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let agent_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            if agent_id.is_empty() {
                                return invalid("agent_id is required".into());
                            }
                            match st.list_runtime_state(&tenant, &agent_id) {
                                Ok(rows) => match serde_json::to_vec(&rows) {
                                    Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                    Err(e) => invalid(format!("rig.runtime_state.get encode: {e}")),
                                },
                                Err(e) => invalid(format!("rig.runtime_state.get: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `rig.runtime_state.list` — the persisted adapter runtime state
            // for EVERY agent in the caller's Guild
            // (`GET /v1/runs/runtime-state/list`). The global operator recovery
            // list: it lets the Settings hub surface all persisted adapter
            // sessions without first knowing an agent id. Optional arg is JSON
            // `{"limit": <n>}` (or empty for the default page); the limit is
            // clamped store-side. Tenant-scoped: only the caller's Guild rows.
            // Returns `{"rows": [...]}`, newest first.
            let st = store.clone();
            bridge.register(
                "rig.runtime_state.list",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            // Empty args → default page; otherwise an optional
                            // `{"limit": n}` (0 also means "default page").
                            let raw = String::from_utf8_lossy(&ctx.args);
                            let trimmed = raw.trim();
                            let limit = if trimmed.is_empty() {
                                0usize
                            } else {
                                match serde_json::from_str::<serde_json::Value>(trimmed) {
                                    Ok(v) => v
                                        .get("limit")
                                        .and_then(|x| x.as_u64())
                                        .map(|n| n as usize)
                                        .unwrap_or(0),
                                    Err(e) => {
                                        return invalid(format!(
                                            "rig.runtime_state.list: invalid JSON: {e}"
                                        ));
                                    }
                                }
                            };
                            match st.list_runtime_state_for_tenant(&tenant, limit) {
                                Ok(rows) => {
                                    let body = serde_json::json!({ "rows": rows });
                                    match serde_json::to_vec(&body) {
                                        Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                        Err(e) => {
                                            invalid(format!("rig.runtime_state.list encode: {e}"))
                                        }
                                    }
                                }
                                Err(e) => invalid(format!("rig.runtime_state.list: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `rig.runtime_state.reset` — forget persisted adapter runtime
            // state (`POST /v1/runs/runtime-state/reset`). Arg is JSON
            // `{"agent_id":"...","brief_key":"..."?}`; with `brief_key` the
            // reset is scoped to that one Brief, otherwise the whole agent.
            // Tenant-scoped. Returns `{"removed": <count>}`.
            let st = store.clone();
            bridge.register(
                "rig.runtime_state.reset",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let tenant = ctx.tenant_id_or_default().to_string();
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            let v: serde_json::Value = match serde_json::from_slice(&ctx.args) {
                                Ok(v) => v,
                                Err(e) => {
                                    return invalid(format!(
                                        "rig.runtime_state.reset: invalid JSON: {e}"
                                    ));
                                }
                            };
                            let agent_id = v
                                .get("agent_id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            if agent_id.is_empty() {
                                return invalid("agent_id is required".into());
                            }
                            let brief_key = v
                                .get("brief_key")
                                .and_then(|x| x.as_str())
                                .map(|s| s.trim())
                                .filter(|s| !s.is_empty());
                            let removed = match brief_key {
                                Some(bk) => {
                                    st.reset_runtime_state_for_brief(&tenant, &agent_id, bk)
                                }
                                None => st.reset_runtime_state(&tenant, &agent_id),
                            };
                            match removed {
                                Ok(n) => {
                                    let body = serde_json::json!({ "removed": n });
                                    match serde_json::to_vec(&body) {
                                        Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                        Err(e) => {
                                            invalid(format!("rig.runtime_state.reset encode: {e}"))
                                        }
                                    }
                                }
                                Err(e) => invalid(format!("rig.runtime_state.reset: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `maintenance.summary` — operator storage + run-ledger overview
            // (`GET /v1/maintenance/summary`). Bounded, symlink-skipping,
            // never scans the repo, handles a missing workspace root. No
            // secrets. Operator-global (a single bridge admin), so the run
            // counts are not tenant-scoped — disk/log usage is global.
            let st = store.clone();
            bridge.register(
                "maintenance.summary",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |_ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            use crate::nodes::coordinator::maintenance as mnt;
                            let cfg = st.run_workspace_config();
                            let root = st.run_workspace_root().to_path_buf();
                            let scan = mnt::scan_run_workspaces(&root);
                            let stats = st.run_ledger_stats().ok();
                            let inherit = std::env::var("RELIX_RUN_WORKSPACE_MODE")
                                .map(|v| v.trim().eq_ignore_ascii_case("inherit"))
                                .unwrap_or(false);
                            let heartbeat_enabled = std::env::var("RELIX_HEARTBEAT_ENABLED")
                                .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
                                .unwrap_or(false);
                            // Warning thresholds (operator hygiene).
                            const BYTES_WARN: u64 = 1024 * 1024 * 1024; // 1 GiB
                            const COUNT_WARN: usize = 200;
                            let mut warnings: Vec<serde_json::Value> = Vec::new();
                            if scan.total_bytes > BYTES_WARN {
                                warnings.push(serde_json::json!({"level":"warn","message":
                                    format!("run-workspace storage is large (~{} MiB across {} workspaces) — prune old workspaces", scan.total_bytes / (1024*1024), scan.count)}));
                            }
                            if scan.count > COUNT_WARN {
                                warnings.push(serde_json::json!({"level":"warn","message":
                                    format!("{} run workspaces on disk — prune old ones to reclaim space", scan.count)}));
                            }
                            if inherit {
                                warnings.push(serde_json::json!({"level":"error","message":
                                    "INHERIT mode active — runs execute in the coordinator working directory, not a scoped sandbox"}));
                            }
                            if cfg.context.as_str() == "copy_repo" && !cfg.project_root.is_dir() {
                                warnings.push(serde_json::json!({"level":"error","message":
                                    "copy_repo context is set but the project root does not exist"}));
                            }
                            // Scheduled-cleanup config + the most recent prune.
                            let autoprune = mnt::resolve_autoprune_config();
                            if autoprune.enabled && !autoprune.dry_run && autoprune.delete_workspaces {
                                warnings.push(serde_json::json!({"level":"warn","message":
                                    format!("scheduled cleanup is enabled in REAL-DELETE mode (every {}h, older than {}d, keep {}) — old workspaces are removed automatically", autoprune.interval_secs / 3600, autoprune.older_than_days, autoprune.keep_latest)}));
                            }
                            let last_prune = st.list_maintenance_audit(1).ok().and_then(|v| v.into_iter().next());
                            if let Some(lp) = &last_prune
                                && (lp.status == "refused" || lp.status == "failed")
                            {
                                warnings.push(serde_json::json!({"level":"error","message":
                                    format!("the last cleanup ({}) {}: {}", lp.trigger, lp.status, lp.note.clone().unwrap_or_default())}));
                            }
                            let body = serde_json::json!({
                                "workspace": {
                                    "root": scan.root,
                                    "exists": scan.exists,
                                    "count": scan.count,
                                    "total_bytes": scan.total_bytes,
                                    "oldest": scan.oldest,
                                    "newest": scan.newest,
                                    "truncated": scan.truncated,
                                },
                                "config": {
                                    "context": cfg.context.as_str(),
                                    "project_root": cfg.project_root.to_string_lossy(),
                                    "inherit": inherit,
                                    "heartbeat_enabled": heartbeat_enabled,
                                },
                                "ledger": stats,
                                "policy": {
                                    "default_older_than_days": mnt::DEFAULT_PRUNE_OLDER_THAN_DAYS,
                                    "default_keep_latest": mnt::DEFAULT_PRUNE_KEEP_LATEST,
                                },
                                "autoprune": autoprune,
                                "last_prune": last_prune,
                                "warnings": warnings,
                            });
                            match serde_json::to_vec(&body) {
                                Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("maintenance.summary encode: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `maintenance.prune` — safe prune of OLD run workspaces (and,
            // optionally, the verbose log rows of pruned runs)
            // (`POST /v1/maintenance/prune`). Dry-run by default; a real
            // delete is explicit (`dry_run:false`). Never touches a running
            // run's workspace, never follows symlinks, refuses an unsafe
            // root, only operates under the configured workspace root.
            let st = store.clone();
            bridge.register(
                "maintenance.prune",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            use crate::nodes::coordinator::maintenance as mnt;
                            let invalid = |c: String| {
                                crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::INVALID_ARGS,
                                        cause: c,
                                        retry_hint: 0,
                                        retry_after: None,
                                    },
                                )
                            };
                            // Parse the JSON body (all fields optional).
                            let v: serde_json::Value = if ctx.args.is_empty() {
                                serde_json::json!({})
                            } else {
                                match serde_json::from_slice(&ctx.args) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        return invalid(format!(
                                            "maintenance.prune: bad body: {e}"
                                        ));
                                    }
                                }
                            };
                            let dry_run =
                                v.get("dry_run").and_then(|x| x.as_bool()).unwrap_or(true);
                            let older_than_days = v
                                .get("older_than_days")
                                .and_then(|x| x.as_u64())
                                .unwrap_or(mnt::DEFAULT_PRUNE_OLDER_THAN_DAYS);
                            let keep_latest = v
                                .get("keep_latest")
                                .and_then(|x| x.as_u64())
                                .map(|n| n as usize)
                                .unwrap_or(mnt::DEFAULT_PRUNE_KEEP_LATEST);
                            let delete_workspaces = v
                                .get("delete_workspaces")
                                .and_then(|x| x.as_bool())
                                .unwrap_or(true);
                            let delete_events = v
                                .get("delete_events")
                                .and_then(|x| x.as_bool())
                                .unwrap_or(false);
                            let delete_artifacts = v
                                .get("delete_artifacts")
                                .and_then(|x| x.as_bool())
                                .unwrap_or(false);

                            // Run the prune through the SHARED engine, which
                            // records a durable audit row for EVERY attempt
                            // (dry-run / refusal / failure / success).
                            let outcome = match mnt::execute_prune(
                                &st,
                                "manual",
                                older_than_days,
                                keep_latest,
                                delete_workspaces,
                                delete_events,
                                delete_artifacts,
                                dry_run,
                            ) {
                                Ok(o) => o,
                                Err(e) => {
                                    return invalid(format!("maintenance.prune refused: {e}"));
                                }
                            };
                            if dry_run {
                                tracing::info!(
                                    candidates = outcome.report.to_delete.len(),
                                    bytes = outcome.report.to_delete_bytes,
                                    audit_id = outcome.audit_id,
                                    "maintenance.prune dry-run"
                                );
                            } else {
                                tracing::warn!(
                                    deleted_workspaces = outcome.report.deleted_workspaces,
                                    deleted_bytes = outcome.report.deleted_bytes,
                                    events_deleted = outcome.events_deleted,
                                    artifacts_deleted = outcome.artifacts_deleted,
                                    audit_id = outcome.audit_id,
                                    "maintenance.prune executed"
                                );
                            }
                            let mut body = serde_json::to_value(&outcome.report)
                                .unwrap_or(serde_json::json!({}));
                            if let Some(obj) = body.as_object_mut() {
                                obj.insert(
                                    "events_deleted".into(),
                                    serde_json::json!(outcome.events_deleted),
                                );
                                obj.insert(
                                    "artifacts_deleted".into(),
                                    serde_json::json!(outcome.artifacts_deleted),
                                );
                                obj.insert("audit_id".into(), serde_json::json!(outcome.audit_id));
                            }
                            match serde_json::to_vec(&body) {
                                Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                Err(e) => invalid(format!("maintenance.prune encode: {e}")),
                            }
                        }
                    },
                )),
            );
        }
        {
            // `maintenance.audit` — recent maintenance-audit rows, newest
            // first (`GET /v1/maintenance/audit?limit=N`). Arg = the limit
            // (defaults to 50). Auth-gated by the bridge middleware.
            let st = store.clone();
            bridge.register(
                "maintenance.audit",
                std::sync::Arc::new(crate::dispatch::FnHandler(
                    move |ctx: crate::dispatch::InvocationCtx| {
                        let st = st.clone();
                        async move {
                            let limit = String::from_utf8_lossy(&ctx.args)
                                .trim()
                                .parse::<i64>()
                                .unwrap_or(50);
                            match st.list_maintenance_audit(limit) {
                                Ok(rows) => match serde_json::to_vec(&rows) {
                                    Ok(b) => crate::dispatch::HandlerOutcome::Ok(b),
                                    Err(e) => crate::dispatch::HandlerOutcome::Err(
                                        relix_core::types::ErrorEnvelope {
                                            kind:
                                                relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                            cause: format!("maintenance.audit encode: {e}"),
                                            retry_hint: 1,
                                            retry_after: None,
                                        },
                                    ),
                                },
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("maintenance.audit: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    },
                )),
            );
        }
        // Scheduled cleanup loop (opt-in): when
        // `RELIX_MAINTENANCE_AUTOPRUNE_ENABLED` is set, a timer periodically
        // runs `maintenance::autoprune_tick` (trigger `scheduled`), which
        // honors the same safety rules as the manual prune and records an
        // audit row every tick. DRY-RUN by default — a real delete needs
        // `RELIX_MAINTENANCE_AUTOPRUNE_DRY_RUN=false`.
        {
            let autoprune = crate::nodes::coordinator::maintenance::resolve_autoprune_config();
            if autoprune.enabled {
                let task_store = store.clone();
                let interval_secs = autoprune.interval_secs;
                tokio::spawn(async move {
                    let mut ticker =
                        tokio::time::interval(std::time::Duration::from_secs(interval_secs));
                    // Skip the immediate first tick so boot isn't a prune.
                    ticker.tick().await;
                    loop {
                        ticker.tick().await;
                        let ts = task_store.clone();
                        let outcome = tokio::task::spawn_blocking(move || {
                            crate::nodes::coordinator::maintenance::autoprune_tick(&ts)
                        })
                        .await;
                        match outcome {
                            Ok(Ok(Some(o))) => tracing::info!(
                                dry_run = o.report.dry_run,
                                deleted = o.report.deleted_workspaces,
                                bytes = o.report.deleted_bytes,
                                audit_id = o.audit_id,
                                "maintenance: scheduled autoprune tick"
                            ),
                            Ok(Ok(None)) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(error = %e, "maintenance: autoprune refused")
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "maintenance: autoprune join error")
                            }
                        }
                    }
                });
                tracing::info!(
                    interval_secs = autoprune.interval_secs,
                    dry_run = autoprune.dry_run,
                    older_than_days = autoprune.older_than_days,
                    keep_latest = autoprune.keep_latest,
                    "coordinator startup: scheduled cleanup loop spawned (RELIX_MAINTENANCE_AUTOPRUNE_ENABLED)"
                );
            }
        }
        // PHASE 3 (heartbeat loop): the live dispatch tick. Opt-in
        // via RELIX_HEARTBEAT_ENABLED (off by default so it never
        // surprises an operator). When on, a timer polls the ready
        // Briefs and runs each on its Operative's Rig via
        // `heartbeat::dispatch_batch`. An Operative with no Rig
        // configured (or an unknown one) resolves to None — the
        // Brief is left untouched (the Desk surfaces it) — so the
        // loop is inert until real Rigs are registered + chosen.
        if std::env::var("RELIX_HEARTBEAT_ENABLED")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
        {
            let interval_secs = std::env::var("RELIX_HEARTBEAT_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(10);
            let batch = std::env::var("RELIX_HEARTBEAT_BATCH")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(16);
            let lease_secs: i64 = 300;
            let task_store = store.clone();
            let ag_store = agent_store.clone();
            let registry = rig_registry.clone();
            // Live spend ledger for the Allowance hard-stop gate
            // (relix-company-model §3.6/§5.2D). `None` when metrics are
            // disabled, in which case only an explicit zero Allowance
            // hard-stops (a positive cap can't be spend-checked).
            let metrics_query = metrics.map(|m| m.query.clone());
            // SpineStore for the Guild-level budget hard-stop on the autonomous
            // path (relix-company-model §6.6): resolve a Brief's Guild + its
            // monthly budget. `None` when the SpineStore failed to open, in
            // which case the Guild gate is inert (per-Operative Allowance still
            // enforced).
            let spine_store_hb = spine_store_for_agent_caps.clone();
            // Per-run bridge-back tokens the dispatch loop mints +
            // injects so a running agent can call Relix's API back.
            let bridge_tokens = crate::rig::bridge::BridgeTokenStore::global();
            tokio::spawn(async move {
                let mut ticker =
                    tokio::time::interval(std::time::Duration::from_secs(interval_secs));
                loop {
                    ticker.tick().await;
                    let ts = task_store.clone();
                    let ags = ag_store.clone();
                    let reg = registry.clone();
                    let bt = bridge_tokens.clone();
                    let mq = metrics_query.clone();
                    let spine = spine_store_hb.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        let ags_for_timer = ags.clone();
                        let ags_for_caps = ags.clone();
                        let ags_for_budget = ags.clone();
                        let ts_for_budget = ts.clone();
                        let spine_for_budget = spine.clone();
                        crate::nodes::coordinator::heartbeat::dispatch_batch_with_policy(
                            &ts,
                            batch,
                            lease_secs,
                            Some(&bt),
                            move |card| {
                                let Some(assignee) = card.assignee_agent_id.as_deref() else {
                                    return false;
                                };
                                match ags_for_timer.get_agent(assignee) {
                                    Ok(Some(agent)) if agent.status == "active" => {
                                        if !agent.wake_on_timer {
                                            tracing::debug!(
                                                agent_id = %assignee,
                                                brief_id = %card.task_id,
                                                "heartbeat: timer wake disabled for Operative"
                                            );
                                            return false;
                                        }
                                        true
                                    }
                                    Ok(Some(agent)) => {
                                        tracing::warn!(
                                            agent_id = %assignee,
                                            status = %agent.status,
                                            brief_id = %card.task_id,
                                            "heartbeat: refusing timer wake for non-active Operative"
                                        );
                                        false
                                    }
                                    Ok(None) => false,
                                    Err(e) => {
                                        tracing::warn!(
                                            agent_id = %assignee,
                                            error = %e,
                                            "heartbeat: failed to read Operative for timer wake"
                                        );
                                        false
                                    }
                                }
                            },
                            move |agent_id| {
                                ags_for_caps
                                    .get_agent(agent_id)
                                    .ok()
                                    .flatten()
                                    .filter(|a| a.status == "active")
                                    .map(|a| a.max_concurrent_runs)
                                    .unwrap_or(0)
                            },
                            // Allowance hard-stop gate
                            // (relix-company-model §3.6/§5.2D): refuse to
                            // dispatch a Brief whose Operative is over its
                            // monthly Allowance or explicitly hard-stopped
                            // (allowance = 0). Spend is the Operative's
                            // month-to-date cost from the metrics ledger.
                            // Autonomous budget gate (relix-company-model
                            // §3.6/§5.2D per-Operative Allowance + §6/§6.6 Guild
                            // budget): the per-Operative hard-stop is authoritative
                            // and the Guild ceiling is ADDITIVE on top of it. Both
                            // read the SAME metrics ledger + canonical calendar-
                            // month window (`heartbeat::allowance_window`) the
                            // Action Center reports, and the Guild spend is summed
                            // ONLY over the Brief's own Guild (tenant-safe).
                            move |card| {
                                let now_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as i64)
                                    .unwrap_or(0);
                                crate::nodes::coordinator::heartbeat::dispatch_budget_admits(
                                    card,
                                    &ts_for_budget,
                                    &ags_for_budget,
                                    spine_for_budget.as_deref(),
                                    mq.as_ref(),
                                    now_ms,
                                )
                            },
                            |card| {
                                let assignee = card.assignee_agent_id.as_deref()?;
                                let agent = ags.get_agent(assignee).ok().flatten()?;
                                if agent.status != "active" {
                                    tracing::warn!(
                                        agent_id = %assignee,
                                        status = %agent.status,
                                        brief_id = %card.task_id,
                                        "heartbeat: refusing to dispatch non-active Operative"
                                    );
                                    return None;
                                }
                                let preferred = agent.rig;
                                reg.resolve(preferred.as_deref())
                            },
                            |card| {
                                // Inject the assigned Operative's charter
                                // (instruction_bundle) as trusted, operator-
                                // authored context ahead of the Brief body.
                                let charter = card
                                    .assignee_agent_id
                                    .as_deref()
                                    .and_then(|a| ags.get_agent(a).ok().flatten())
                                    .map(|p| p.instruction_bundle)
                                    .filter(|c| !c.trim().is_empty());
                                ts.compose_brief_prompt_with_charter(
                                    &card.task_id,
                                    10,
                                    charter.as_deref(),
                                )
                            },
                            // Carry the assigned Operative's stored model/effort
                            // preference into the autonomous run so a supported
                            // CLI Rig runs on it (relix-agent-adapters.md
                            // §3.2/§3.3). The caller owns the agent lookup (as
                            // with `resolve_rig`), keeping dispatch decoupled.
                            |card| {
                                let agent = card
                                    .assignee_agent_id
                                    .as_deref()
                                    .and_then(|a| ags.get_agent(a).ok().flatten());
                                match agent {
                                    Some(a) => crate::nodes::coordinator::heartbeat::RunModelPrefs::new(
                                        a.model_preference,
                                        a.reasoning_effort,
                                    ),
                                    None => crate::nodes::coordinator::heartbeat::RunModelPrefs::default(),
                                }
                            },
                        )
                    })
                    .await;
                    match outcome {
                        Ok(Ok(records)) if !records.is_empty() => tracing::debug!(
                            count = records.len(),
                            "heartbeat: dispatch tick processed Briefs"
                        ),
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "heartbeat: dispatch tick failed")
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "heartbeat: dispatch task join error")
                        }
                    }
                    // Periodic stale-run sweep: recover any `running` Shift
                    // whose child is gone (a dead run thread that never closed
                    // its row) once it is older than its lease + grace, while
                    // leaving genuinely in-flight runs alone — those are either
                    // registered as live in the CancelRegistry or younger than
                    // the threshold. Marks each `interrupted` and frees its
                    // Claim (only if still owned by that run).
                    let sweep_store = task_store.clone();
                    let sweep_threshold = lease_secs + 60;
                    let _ = tokio::task::spawn_blocking(move || {
                        let live = crate::rig::CancelRegistry::global().live_ids();
                        match sweep_store.recover_stale_runs(&live, sweep_threshold) {
                            Ok(ids) if !ids.is_empty() => tracing::warn!(
                                recovered = ids.len(),
                                "heartbeat: recovered stale `running` brief runs as `interrupted`"
                            ),
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "heartbeat: stale-run sweep failed")
                            }
                        }
                    })
                    .await;
                    // Defensive hygiene: reap any bridge tokens that
                    // outlived their Shift (e.g. a panicked dispatch
                    // that never reached its revoke).
                    let reaped = bridge_tokens.sweep_expired();
                    if reaped > 0 {
                        tracing::debug!(reaped, "heartbeat: swept expired bridge tokens");
                    }
                }
            });
            tracing::info!(
                interval_secs,
                batch,
                "coordinator startup: heartbeat dispatch loop spawned (RELIX_HEARTBEAT_ENABLED)"
            );
        }
        // STAGE-2 OPT-IN autonomous retry lane (execution-and-issue §3.3 /
        // §3.3b Stage-2a). Default OFF via RELIX_AUTONOMOUS_RECOVERY so it never
        // surprises an operator. When on, a timer selects retryable failed/
        // interrupted Shifts (already diagnosed `retryable` with budget
        // remaining) and re-opens EXACTLY ONE child each through the SAME guarded
        // `open_retry_child` path the operator one-click uses (same Claim,
        // workspace, ledger, model prefs, Codex resume, duplicate-child guard) —
        // NOT a second retry path. Bounded per tick (RELIX_AUTONOMOUS_RECOVERY_MAX,
        // default 1, clamp 1..=10) and idempotent (the duplicate guard means a
        // re-tick opens no second child). Independent of RELIX_HEARTBEAT_ENABLED.
        // NO LLM diagnostic pass + NO provider quota polling in this slice.
        if crate::nodes::coordinator::heartbeat::parse_autonomous_recovery_enabled(
            std::env::var("RELIX_AUTONOMOUS_RECOVERY").ok().as_deref(),
        ) {
            let recovery_max = crate::nodes::coordinator::heartbeat::parse_autonomous_recovery_max(
                std::env::var("RELIX_AUTONOMOUS_RECOVERY_MAX")
                    .ok()
                    .as_deref(),
            );
            let recovery_interval_secs = std::env::var("RELIX_AUTONOMOUS_RECOVERY_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(30);
            let lease_secs: i64 = 300;
            let task_store = store.clone();
            let ag_store = agent_store.clone();
            let registry = rig_registry.clone();
            let metrics_query = metrics.map(|m| m.query.clone());
            let spine_store_rec = spine_store_for_agent_caps.clone();
            let bridge_tokens = crate::rig::bridge::BridgeTokenStore::global();
            tokio::spawn(async move {
                let mut ticker =
                    tokio::time::interval(std::time::Duration::from_secs(recovery_interval_secs));
                loop {
                    ticker.tick().await;
                    let ts = task_store.clone();
                    let ags = ag_store.clone();
                    let reg = registry.clone();
                    let bt = bridge_tokens.clone();
                    let mq = metrics_query.clone();
                    let spine = spine_store_rec.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        // Per-candidate policy (decoupled from the tick, exactly
                        // like the heartbeat's resolve_rig/build_prompt closures):
                        // resolve the assignee's Rig + charter-aware prompt +
                        // model prefs IDENTICALLY to the operator retry, and SKIP
                        // (no retry, quietly) when the Operative is paused/
                        // terminated, its timer wake is off, or it is over budget
                        // — so the autonomous lane respects the SAME budget
                        // hard-stop the autonomous dispatch does.
                        let decide = |cand: &crate::nodes::coordinator::RetryCandidate| {
                            use crate::nodes::coordinator::heartbeat::{
                                RetryDecision, RetryInputs, RunModelPrefs,
                            };
                            let Some(card) = ts.brief_card(&cand.brief_id).ok().flatten() else {
                                return RetryDecision::Skip;
                            };
                            let Some(assignee) = card.assignee_agent_id.as_deref() else {
                                return RetryDecision::Skip;
                            };
                            let Some(agent) = ags.get_agent(assignee).ok().flatten() else {
                                return RetryDecision::Skip;
                            };
                            if agent.status != "active" || !agent.wake_on_timer {
                                return RetryDecision::Skip;
                            }
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            if !matches!(
                                crate::nodes::coordinator::heartbeat::dispatch_budget_admits(
                                    &card,
                                    &ts,
                                    &ags,
                                    spine.as_deref(),
                                    mq.as_ref(),
                                    now_ms,
                                ),
                                crate::nodes::coordinator::heartbeat::BudgetAdmission::Allow
                            ) {
                                return RetryDecision::Skip;
                            }
                            let charter = agent.instruction_bundle.clone();
                            let charter = if charter.trim().is_empty() {
                                None
                            } else {
                                Some(charter)
                            };
                            let prompt = ts.compose_brief_prompt_with_charter(
                                &card.task_id,
                                10,
                                charter.as_deref(),
                            );
                            RetryDecision::Proceed(RetryInputs {
                                preferred_rig: agent.rig.clone(),
                                prompt,
                                prefs: RunModelPrefs::new(
                                    agent.model_preference.clone(),
                                    agent.reasoning_effort.clone(),
                                ),
                            })
                        };
                        // tenant=None: recover all Guilds, each candidate retried
                        // under its OWN derived tenant (no cross-Guild leak).
                        crate::nodes::coordinator::heartbeat::autonomous_recovery_tick(
                            &ts,
                            &reg,
                            Some(&bt),
                            lease_secs,
                            recovery_max,
                            None,
                            decide,
                        )
                    })
                    .await;
                    match outcome {
                        Ok(Ok(records)) => {
                            let opened = records.iter().filter(|r| r.outcome == "opened").count();
                            if opened > 0 {
                                tracing::info!(
                                    opened,
                                    considered = records.len(),
                                    "autonomous recovery: opened child retries"
                                );
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "autonomous recovery: tick failed")
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "autonomous recovery: tick join error")
                        }
                    }
                }
            });
            tracing::info!(
                interval_secs = recovery_interval_secs,
                max = recovery_max,
                "coordinator startup: autonomous retry lane spawned (RELIX_AUTONOMOUS_RECOVERY)"
            );
        }
        // RUNTIME-CONTROLLABLE autonomous PRIME driver (Prime Runtime Autonomy
        // Switch v1 + company-model §5.4/§8.2 the Action Center "next governed
        // step"; §12.5/§12.5B the Prime planner + Start). Previously the loop
        // only spawned when the boot-time env RELIX_AUTONOMOUS_PRIME was set;
        // now a DORMANT watcher spawns whenever the SpineStore exists, and EACH
        // TICK decides what to drive from the persisted per-Guild runtime
        // setting + the env override — so an operator turns it on/off from the
        // product with no restart:
        //   * env override ON               → drive ALL Guilds (legacy behaviour);
        //   * env off, runtime ON for some  → drive only those Guild(s);
        //   * neither                       → dormant (one cheap SQL read, sleep).
        // Safety is UNCHANGED: ON only wakes the loop over already-APPROVED work
        // — it advances only the safe governed steps `prime.advance` allows
        // (`create_team_plan` / `orchestrate_assign_ready`) and starts ready
        // approved-proposal Briefs through `prime.start` (gated by the SAME
        // autonomous budget hard-stop), and NEVER auto-approves a strategy /
        // hire / spawn / budget / Clearance gate unless a live standing grant
        // covers it. Bounded per tick (RELIX_AUTONOMOUS_PRIME_MAX), idempotent
        // (each tick re-classifies live state), tenant-safe (each candidate
        // under its OWN Guild). Needs the SpineStore (where the runtime setting
        // + approved proposals live); not spawned when it is unavailable.
        if let Some(spine_arc) = spine_store_for_agent_caps.clone() {
            let prime_max = crate::nodes::coordinator::heartbeat::parse_autonomous_prime_max(
                std::env::var("RELIX_AUTONOMOUS_PRIME_MAX").ok().as_deref(),
            );
            let prime_interval_secs = std::env::var("RELIX_AUTONOMOUS_PRIME_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(30);
            // The Rig the STANDING-AUTHORITY hire-approve binds (default
            // safe-local `echo`). Passed through unvalidated on purpose — the
            // tick refuses/skips a hire on an unknown Rig rather than silently
            // binding a bad one, so a typo surfaces as a pending hire.
            let prime_hire_rig =
                crate::nodes::coordinator::agent::prime_driver::configured_autonomous_hire_rig();
            let env_at_boot = crate::nodes::coordinator::heartbeat::parse_autonomous_prime_enabled(
                std::env::var("RELIX_AUTONOMOUS_PRIME").ok().as_deref(),
            );
            let task_store = store.clone();
            let ag_store = agent_store.clone();
            let registry = rig_registry.clone();
            let metrics_query = metrics.map(|m| m.query.clone());
            // Prime Deliberation v1 wiring. The loop reuses the coordinator's
            // EXISTING outbound mesh client (the one the alert sink wires from
            // `[peers]`) to reach the AI peer — no provider key ever enters the
            // coordinator; this is just the existing governed `ai.chat` call. The
            // AI-peer alias + session are read once at boot; the
            // `RELIX_PRIME_LLM_DELIBERATION` switch is re-read each tick (like the
            // env override). When the mesh cell is absent / unpopulated, the loop
            // passes no decider and every tick is deterministic.
            let prime_alert_cell = metrics.map(|m| m.alert_mesh_cell.clone());
            let prime_ai_peer = crate::nodes::coordinator::agent::prime_driver::prime_ai_peer();
            let prime_llm_session =
                crate::nodes::coordinator::agent::prime_driver::prime_llm_session();
            tokio::spawn(async move {
                let mut ticker =
                    tokio::time::interval(std::time::Duration::from_secs(prime_interval_secs));
                loop {
                    ticker.tick().await;
                    let ts = task_store.clone();
                    let ags = ag_store.clone();
                    let reg = registry.clone();
                    let mq = metrics_query.clone();
                    let spine = spine_arc.clone();
                    let hire_rig = prime_hire_rig.clone();
                    // Captured for the live deliberation decider built inside the
                    // blocking tick (it bridges the async mesh call via this handle).
                    let prime_handle = tokio::runtime::Handle::current();
                    let prime_alert_cell = prime_alert_cell.clone();
                    let prime_ai_peer = prime_ai_peer.clone();
                    let prime_llm_session = prime_llm_session.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        use crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider;
                        use crate::nodes::coordinator::agent::prime_driver::{
                            AutonomyDrive, MeshAiDecider, RUNTIME_KEY_AUTONOMOUS_PRIME,
                            autonomous_prime_tick, parse_prime_llm_deliberation,
                            parse_prime_llm_orchestration, parse_prime_llm_plan_package,
                            parse_prime_llm_prioritization, parse_prime_llm_strategy_draft,
                            parse_prime_plan_package_trigger, plan_autonomy_drive,
                        };
                        // Re-read all three Prime LLM switches each tick: deliberation
                        // (action choice), strategy draft (proposed strategy body
                        // authoring), and prioritization (queue order among legal
                        // candidates). When ANY is on AND a populated coordinator mesh
                        // client exists, build the live decider (the SAME decider serves
                        // all three); otherwise leave it None (deterministic).
                        let prime_llm = parse_prime_llm_deliberation(
                            std::env::var("RELIX_PRIME_LLM_DELIBERATION")
                                .ok()
                                .as_deref(),
                        );
                        let prime_strategy_llm = parse_prime_llm_strategy_draft(
                            std::env::var("RELIX_PRIME_LLM_STRATEGY_DRAFT")
                                .ok()
                                .as_deref(),
                        );
                        let prime_prioritization = parse_prime_llm_prioritization(
                            std::env::var("RELIX_PRIME_LLM_PRIORITIZATION")
                                .ok()
                                .as_deref(),
                        );
                        let prime_orchestration = parse_prime_llm_orchestration(
                            std::env::var("RELIX_PRIME_LLM_ORCHESTRATION")
                                .ok()
                                .as_deref(),
                        );
                        let prime_plan_package = parse_prime_llm_plan_package(
                            std::env::var("RELIX_PRIME_LLM_PLAN_PACKAGE")
                                .ok()
                                .as_deref(),
                        );
                        // WHEN plan-package authoring fires (tail vs before_execute);
                        // inert unless the master plan-package switch is on.
                        let prime_plan_package_trigger = parse_prime_plan_package_trigger(
                            std::env::var("RELIX_PRIME_PLAN_PACKAGE_TRIGGER")
                                .ok()
                                .as_deref(),
                        );
                        let prime_decider = if prime_llm
                            || prime_strategy_llm
                            || prime_prioritization
                            || prime_orchestration
                            || prime_plan_package
                        {
                            prime_alert_cell.as_ref().and_then(|c| c.get()).map(|ctx| {
                                MeshAiDecider::new(
                                    prime_handle.clone(),
                                    ctx.mesh.clone(),
                                    ctx.identity.clone(),
                                    prime_ai_peer.clone(),
                                    prime_llm_session.clone(),
                                    30,
                                    None,
                                )
                            })
                        } else {
                            None
                        };
                        let prime_ai: Option<&dyn PrimeAiDecider> =
                            prime_decider.as_ref().map(|d| d as &dyn PrimeAiDecider);
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        // Re-read the env override each tick (cheap; normally
                        // static, but honour a live change). When env is off the
                        // persisted per-Guild runtime setting decides which
                        // Guilds (if any) to drive — a runtime-off Guild is never
                        // driven.
                        let env_enabled =
                            crate::nodes::coordinator::heartbeat::parse_autonomous_prime_enabled(
                                std::env::var("RELIX_AUTONOMOUS_PRIME").ok().as_deref(),
                            );
                        let enabled_tenants = if env_enabled {
                            Vec::new()
                        } else {
                            spine
                                .list_tenants_with_runtime_bool(RUNTIME_KEY_AUTONOMOUS_PRIME)
                                .unwrap_or_default()
                        };
                        let mut records = Vec::new();
                        match plan_autonomy_drive(env_enabled, enabled_tenants) {
                            // Nothing enabled — do nothing this tick.
                            AutonomyDrive::Dormant => {}
                            // Env override on — drive ALL Guilds (tenant=None),
                            // each candidate under its own derived tenant.
                            AutonomyDrive::AllGuilds => {
                                let mut r = autonomous_prime_tick(
                                    &ags,
                                    &spine,
                                    &ts,
                                    &reg,
                                    mq.as_ref(),
                                    now_ms,
                                    prime_max,
                                    None,
                                    &hire_rig,
                                    prime_ai,
                                    prime_llm,
                                    prime_strategy_llm,
                                    prime_prioritization,
                                    prime_orchestration,
                                    prime_plan_package,
                                    prime_plan_package_trigger,
                                )?;
                                records.append(&mut r);
                            }
                            // Env off — drive ONLY the runtime-enabled Guild(s),
                            // each scoped to its own tenant (bounded per Guild).
                            AutonomyDrive::Tenants(tenants) => {
                                for t in tenants {
                                    let mut r = autonomous_prime_tick(
                                        &ags,
                                        &spine,
                                        &ts,
                                        &reg,
                                        mq.as_ref(),
                                        now_ms,
                                        prime_max,
                                        Some(&t),
                                        &hire_rig,
                                        prime_ai,
                                        prime_llm,
                                        prime_strategy_llm,
                                        prime_prioritization,
                                        prime_orchestration,
                                        prime_plan_package,
                                        prime_plan_package_trigger,
                                    )?;
                                    records.append(&mut r);
                                }
                            }
                        }
                        Ok::<_, String>(records)
                    })
                    .await;
                    match outcome {
                        Ok(Ok(records)) => {
                            let advanced =
                                records.iter().filter(|r| r.outcome == "advanced").count();
                            let started = records.iter().filter(|r| r.outcome == "started").count();
                            if advanced > 0 || started > 0 {
                                tracing::info!(
                                    advanced,
                                    started,
                                    considered = records.len(),
                                    "autonomous prime: drove approved work forward"
                                );
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "autonomous prime: tick failed")
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "autonomous prime: tick join error")
                        }
                    }
                }
            });
            tracing::info!(
                interval_secs = prime_interval_secs,
                max = prime_max,
                env_override = env_at_boot,
                "coordinator startup: autonomous Prime watcher spawned (runtime-controllable; \
                 env override + persisted per-Guild runtime toggle)"
            );
        }
        // NOT-DONE 2: spawn the legacy-token orphaned-task fail
        // pass in the BACKGROUND so it does not block the
        // controller from accepting requests. The pass:
        //
        //   * skips entirely when the `startup_tasks` ledger
        //     records a prior successful completion;
        //   * resumes from `last_processed_id` after process
        //     interruption (the cursor is persisted every
        //     `LEGACY_TOKEN_PASS_PROGRESS_INTERVAL` rows AND
        //     at end-of-run);
        //   * yields between rows so a thousand-row pass does
        //     not starve other tokio tasks;
        //   * logs INFO at start, every checkpoint, and at
        //     completion;
        //   * per-row failures (task lookup error / state
        //     machine reject) log at WARN and skip — they
        //     never abort the whole pass.
        {
            let agent_store_for_pass = agent_store.clone();
            let task_store_for_pass = store.clone();
            let clock_for_pass = bridge.clock();
            tokio::spawn(async move {
                let _ = run_legacy_token_orphaned_task_fail_pass(
                    agent_store_for_pass,
                    task_store_for_pass,
                    clock_for_pass,
                )
                .await;
            });
        }
        // DEFERRED 1: resolve the operator-configured token TTL
        // ONCE at register time so the cap closure captures a
        // clamped value. The startup logs the effective TTL
        // alongside the source of the value so operators can
        // tell at a glance whether the configured value was
        // accepted as-is or clamped.
        let raw_ttl_secs = cfg
            .approval
            .as_ref()
            .and_then(|a| a.approval_token_ttl_secs);
        let effective_ttl_secs =
            crate::nodes::coordinator::agent::handlers::clamp_approval_token_ttl_secs(raw_ttl_secs);
        match raw_ttl_secs {
            Some(v) if v == effective_ttl_secs => tracing::info!(
                ttl_secs = effective_ttl_secs,
                "approval: token TTL = {effective_ttl_secs}s (operator-configured)"
            ),
            Some(v) => tracing::info!(
                configured = v,
                clamped = effective_ttl_secs,
                "approval: token TTL clamped to {effective_ttl_secs}s (configured {v}s outside [{}, {}])",
                crate::nodes::coordinator::agent::handlers::APPROVAL_TOKEN_TTL_MIN_SECS,
                crate::nodes::coordinator::agent::handlers::APPROVAL_TOKEN_TTL_MAX_SECS,
            ),
            None => tracing::info!(
                ttl_secs = effective_ttl_secs,
                "approval: token TTL = {effective_ttl_secs}s (default; set [approval] approval_token_ttl_secs to override)"
            ),
        }
        let agent_caps_clock = bridge.clock();
        register_agent_capabilities(
            bridge,
            agent_store.clone(),
            store.clone(),
            spine_store_for_agent_caps,
            effective_ttl_secs,
            agent_caps_clock,
            manifest.descriptor_cache(),
            // Authoritative live-spend ledger for the Action Center budget
            // alerts — the SAME query the heartbeat Allowance gate uses.
            // `Option<&MetricsBundle>` is `Copy`, so this read does not disturb
            // the heartbeat closure's own `metrics.map(...)` above.
            metrics.map(|m| m.query.clone()),
        );
        let agent_caps: &[(&str, &str, &[&str])] = &[
            (
                "agent.create",
                "Create an agent profile (active immediately — operator/admin only; an Operative actor is refused and routed to agent.request_hire). Arg: name|role|title|department|team|created_by|subject_id|risk_ceiling.",
                &["agent", "persist"],
            ),
            (
                "agent.request_hire",
                "Gated creation: mint an Operative `pending` (inert until approved). An Operative actor needs the spawn Key (can_spawn_agents); spawn_route=lead/founder returns a `clearance:` note. Same arg shape as agent.create. Returns the new agent_id.",
                &["agent", "persist"],
            ),
            (
                "agent.request_hire_for_mandate",
                "Strategy-gated hire: mandate_id|name|role|title|department|team|created_by|subject_id|risk_ceiling. Refuses until the Mandate strategy is approved.",
                &["agent", "persist", "governance"],
            ),
            (
                "mandate.team_plan",
                "Prime team-build foundation (governed, not autonomous): mandate_id|description?|roles? where roles is a CSV of `role` or `role:subject_id`. Requires approved strategy + the actor's spawn Key. Mints pending hires (with spawn Clearances) for roles given an identity; persists the plan and returns a JSON plan {plan_id, status, proposed_roles, pending_hires, clearances, clearance_ids, denials, next_steps}.",
                &["agent", "persist", "governance"],
            ),
            (
                "mandate.team_plan.latest",
                "Read the latest persisted Team Plan for a Mandate as JSON (null if never planned). Arg: mandate_id. Tenant-scoped.",
                &["mandate", "read"],
            ),
            (
                "mandate.team_readiness",
                "Live team readiness for a Mandate: combines the latest plan with current hire/Clearance states. Arg: mandate_id. Returns {missing_roles, pending_clearances, active_agents, pending_hires, blocked_roles, readiness, next_action}. Tenant-scoped.",
                &["mandate", "read"],
            ),
            (
                "mandate.orchestrate",
                "Prime Mandate-to-Brief orchestration (deterministic, non-LLM): mandate_id|mode?|max_briefs?|dry_run? where mode ∈ plan_only/create_briefs/assign_ready. Requires an approved strategy + a ready team; creates an idempotent parent+child Brief tree linked to the Mandate and assigns active agents (assign-Key gated). Returns {ready, blockers, created_briefs, assigned_briefs, existing_briefs, skipped, next_actions}.",
                &["mandate", "persist", "governance"],
            ),
            (
                "mandate.orchestration.latest",
                "Latest persisted orchestration run for a Mandate as JSON (null if never run). Arg: mandate_id. Tenant-scoped.",
                &["mandate", "read"],
            ),
            (
                "mandate.orchestration.list",
                "Recent orchestration runs for a Mandate (newest first). Arg: mandate_id|limit?. Tenant-scoped.",
                &["mandate", "read"],
            ),
            (
                "agent.approve_hire",
                "Approve a pending hire (pending → active). Arg: agent_id.",
                &["agent", "mutate"],
            ),
            (
                "agent.reject_hire",
                "Reject a pending hire (pending → disabled, terminal). Arg: agent_id.",
                &["agent", "mutate"],
            ),
            ("agent.get", "Read one agent profile.", &["agent", "read"]),
            (
                "agent.list",
                "List agent profiles (optionally filtered by subject_id).",
                &["agent", "read"],
            ),
            (
                "agent.update",
                "Update one of {status, role, title, department, team, surface_allowlist, risk_ceiling, allow_categories, deny_categories, allow_sensitivity_tags, deny_sensitivity_tags, approval_required_categories, approval_timeout_secs, reports_to, rig, allowance, max_concurrent_runs, wake_on_timer, wake_on_demand, can_spawn_agents, spawn_route, can_assign_work, assign_scope, assign_allowed_agents, can_manage_work, manage_scope, manage_allowed_agents, can_configure_agents, configure_scope, configure_allowed_agents, secret_allowlist, instruction_bundle}.",
                &["agent", "mutate"],
            ),
            (
                "agent.delete",
                "Soft delete: flip the profile's status to `disabled`.",
                &["agent", "mutate"],
            ),
            (
                "agent.reports",
                "Org tree: the Operatives directly reporting to an agent (the Roster children). Arg: agent_id. One agent_id per line.",
                &["agent", "read"],
            ),
            (
                "agent.peers",
                "Org tree: the Operatives reporting to the same Lead as an agent (its peers, excluding itself). Arg: agent_id. One agent_id per line; empty for an apex.",
                &["agent", "read"],
            ),
            (
                "agent.by_role",
                "Staffing: the active Operatives with a given role (assignable staff). Arg: role. One agent_id per line.",
                &["agent", "read"],
            ),
            (
                "agent.branch",
                "Org tree: every Operative at or below an agent (the manager's Branch / subtree). Arg: agent_id. One agent_id per line.",
                &["agent", "read"],
            ),
            (
                "agent.line",
                "Org tree: the escalation path up from an agent to the apex (the Line / chain of command). Arg: agent_id. One agent_id per line, nearest boss first.",
                &["agent", "read"],
            ),
            (
                "agent.keys",
                "The full Operative profile as JSON (identity + the Keys permission surface + the Lead). Structured read for the per-agent Keys panel. Arg: agent_id.",
                &["agent", "read"],
            ),
            (
                "agent.manages",
                "Delegated-authority check: does a manager manage a target (target in the manager's Branch/subtree)? Arg: manager_id|target_id. Returns true/false.",
                &["agent", "read"],
            ),
            (
                "agent.assign_check",
                "Assign-Key verdict: may `actor` assign a Brief to `assignee` under its Keys? Arg: actor_id|assignee_id. Returns the JSON KeyVerdict (allow/deny + reason). Enforcement counterpart runs at brief.set.",
                &["agent", "read"],
            ),
            (
                "agent.roster_summary",
                "Operative counts by status (active/pending/suspended/disabled) + total, as JSON. No args. The Roster-at-a-glance.",
                &["agent", "read"],
            ),
            (
                "agent.allowance_committed",
                "Total monthly Allowance committed across the active roster, in cents (NULL counts as 0). No args. Pair with guild.get for commitment-vs-budget.",
                &["agent", "read"],
            ),
            (
                "agent.effective_capabilities",
                "Given an agent_id and a peer alias, intersect the peer's manifest with the agent's categorical permissions. Returns one method per line + count=N.",
                &["agent", "read"],
            ),
            (
                "coord.approval.pending",
                "List pending approvals (newest first). Arg: limit (default 20).",
                &["approval", "read"],
            ),
            (
                "brief.clearance_request",
                "Create a pending Clearance linked to a Brief. Arg: brief_id|agent_id|method|category|reason|ttl_secs?.",
                &["approval", "persist", "brief"],
            ),
            (
                "coord.approval.get",
                "Look up one approval by id. Arg: approval_id (raw bytes). Returns a JSON object with every operator-visible field: approval_id, agent_id, subject_id, method, capability_category, reason, requested_at, expires_at, status, decided_at, decided_by, decision_note, task_id, authorized_approvers. Distinguishes `pending` from terminal states (`approved` / `rejected` / `expired` / `consumed` / `legacy_token_expired`); the bridge's `GET /v1/approval/:id` route forwards the JSON verbatim and maps INVALID_ARGS / `not found` to HTTP 404.",
                &["approval", "read"],
            ),
            (
                "coord.approval.decide",
                "Approve or reject a pending approval. Arg: approval_id|approved|decided_by|note OR approval_id|rejected|decided_by|note. Returns `ok|<token>\\n` on approve, `ok\\n` on reject.",
                &["approval", "mutate"],
            ),
            (
                "agent.standing_approval.create",
                "Grant a time-bounded categorical pre-approval. Arg: agent_id|category|expires_at|granted_by|note|path_glob?.",
                &["standing_approval", "persist"],
            ),
            (
                "agent.standing_approval.list",
                "List active + recent standing approvals for an agent.",
                &["standing_approval", "read"],
            ),
            (
                "agent.standing_approval.revoke",
                "Revoke a standing approval by standing_id.",
                &["standing_approval", "mutate"],
            ),
        ];
        for (method, doc, cats) in agent_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }
        tracing::info!("coordinator node: registered agent.* + coord.approval.* capabilities");

        // Auto-expire loop: 60-second tick that scans for
        // pending approvals whose deadline has passed and
        // flips them to `expired` + fails the waiting task.
        {
            let agent_store_for_expire = agent_store.clone();
            let task_store_for_expire = store.clone();
            // NOT-DONE 1: pull the same clock the dispatch
            // bridge installed so the expire sweep + every
            // other TTL surface observe the same time source.
            let clock_for_expire = bridge.clock();
            tokio::spawn(async move {
                run_approval_expire_loop(
                    agent_store_for_expire,
                    task_store_for_expire,
                    clock_for_expire,
                )
                .await;
            });
            tracing::info!("coordinator node: approval auto-expire loop spawned");
        }

        // ── Agent-to-agent messaging ───────────────────────
        // Same coordinator db. Capability handlers + a
        // 5-minute auto-expire sweeper that flips
        // past-ttl messages to `status = expired`.
        let message_store = std::sync::Arc::new(
            crate::nodes::coordinator::messaging::MessageStore::open(&coord_cfg.db_path)
                .map_err(|e| format!("[coordinator] message store open: {e}"))?,
        );
        register_messaging_capabilities(bridge, message_store.clone(), store.clone());
        let msg_caps: &[(&str, &str, &[&str])] = &[
            (
                "msg.send",
                "Send an agent-to-agent message. Arg: \
                 from|to|subject|body|thread_id|reply_to|ttl_secs|origin_surface. \
                 Empty thread_id starts a new thread (uses message_id); empty \
                 ttl_secs defaults to 86400 (24 h).",
                &["messaging", "persist"],
            ),
            (
                "msg.inbox",
                "Read inbox newest-first. Arg: \
                 subject_id|limit|include_read|since_message_id. \
                 limit defaults to 20 (max 100); include_read=1 includes \
                 read messages; since_message_id is a pagination cursor.",
                &["messaging", "read"],
            ),
            (
                "msg.read",
                "Mark a message as read. Arg: message_id|reader_subject_id. \
                 Reader must equal to_subject_id; idempotent on already-read \
                 messages.",
                &["messaging", "mutate"],
            ),
            (
                "msg.thread",
                "List every message in a thread (oldest-first). Arg: \
                 thread_id|subject_id. Caller must be sender or recipient on \
                 at least one message in the thread.",
                &["messaging", "read"],
            ),
            (
                "msg.delete",
                "Soft delete (status=expired). Arg: message_id|subject_id. \
                 Only sender or recipient may delete.",
                &["messaging", "mutate"],
            ),
        ];
        for (method, doc, cats) in msg_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }
        tracing::info!("coordinator node: registered msg.* capabilities");

        // Message auto-expire loop: 5-minute tick.
        {
            let message_store_for_expire = message_store.clone();
            tokio::spawn(async move {
                run_message_expire_loop(message_store_for_expire).await;
            });
            tracing::info!("coordinator node: message auto-expire loop spawned");
        }
        let _ = message_store;

        let coord_caps: &[(&str, &str, &[&str])] = &[
            (
                "task.create",
                "Mint a new Task row in the durable ledger (status=pending).",
                &["task", "persist"],
            ),
            (
                "task.update",
                "Mutate status / result / flow pointer / failure class / trace_id. \
                 Drives the per-attempt timeline as a side effect of status \
                 transitions.",
                &["task", "mutate"],
            ),
            (
                "task.event",
                "Append a free-form event to a Task's chronicle.",
                &["task", "append"],
            ),
            (
                "task.get",
                "Read one Task plus its event chronicle.",
                &["task", "read"],
            ),
            (
                "task.list",
                "Page through Task summaries (limit|offset|status). Most- \
                 recently-updated first.",
                &["task", "read"],
            ),
            (
                "task.count",
                "Total task count, optionally filtered by status. Drives \
                 pagination 'N of M' hints.",
                &["task", "read"],
            ),
            (
                "task.list_cursor",
                "Cursor-paginated task list. Stable under concurrent \
                 inserts/updates; rows are not repeated or skipped \
                 across pages.",
                &["task", "read", "cursor"],
            ),
            (
                "task.export",
                "Archival snapshot of one task: header + attempts + every \
                 chronicle event in a single JSON object. The operator's \
                 save-before-delete artifact.",
                &["task", "read", "export", "operator"],
            ),
            (
                "task.compact_events",
                "Dry-run candidate counter for the chronicle-retention \
                 max-age policy. Counts what *would* be deleted; does not \
                 delete. Only `mode=dry-run` is shipped today (destructive \
                 path gated, see chronicle-retention.md Step 3).",
                &["task", "read", "retention", "operator"],
            ),
            (
                "task.edges",
                "List execution edges that touch the given task (as child \
                 or parent). Phase-1E execution graph primitive — only \
                 `retried_from` is emitted today; other edge types in \
                 the schema are reserved for future runtime primitives.",
                &["task", "read", "graph", "lineage"],
            ),
            (
                "task.recent_edges",
                "Cross-task aggregate of the most recent execution edges. \
                 Newest-first; supports `since_edge_id` cursor for \
                 incremental polling. Operators use this to spot \
                 retry-storm patterns without per-task drill-in.",
                &["task", "read", "graph", "lineage", "operator"],
            ),
            (
                "task.note",
                "Append an operator-authored annotation to a Task's \
                 chronicle as a `task.operator_note` event. The note \
                 becomes part of the immutable history; the author \
                 is taken from the verified caller's subject_id.",
                &["task", "write", "annotate", "operator"],
            ),
            (
                "task.mark_investigation",
                "Toggle the operator-set investigation marker on a \
                 Task. Persists `investigation_marked_at` + optional \
                 `investigation_reason` on the task row and emits a \
                 `task.investigation_marked` / `task.investigation_cleared` \
                 chronicle event. Used to flag tasks that need follow-up.",
                &["task", "write", "annotate", "operator"],
            ),
            (
                "task.pause",
                "Operator-initiated pause. Transitions the task to \
                 `paused` and emits a `task.paused` chronicle event \
                 with the pre-pause status + reason + verified \
                 caller identity. HONEST: no flow-pause primitive \
                 exists yet — a currently-executing flow continues \
                 running and its write-back may overwrite the \
                 `paused` status. Same caveat as `task.cancel`.",
                &["task", "write", "intervene", "operator"],
            ),
            (
                "task.resume",
                "Operator-initiated resume. Refuses any status \
                 other than `paused`. Restores to `pending` so a \
                 subsequent runtime tick can open a new attempt. \
                 Emits a `task.resumed` event recording the \
                 pre-pause status (read from the last `task.paused` \
                 event). Does NOT re-dispatch the flow; the \
                 operator must trigger re-execution via the retry \
                 path if needed.",
                &["task", "write", "intervene", "operator"],
            ),
            (
                "task.lineage",
                "BFS execution-lineage walk from a root task. \
                 Args: `task_id|max_depth`. Returns the set of \
                 related tasks + the edges connecting them. \
                 Today only `retried_from` edges populate the \
                 graph (within-task only); other edge types in \
                 the schema are reserved for future runtime \
                 producers (spawned/delegated_to/parallel_branch/etc.).",
                &["task", "read", "graph", "lineage", "operator"],
            ),
            (
                "task.recent_events",
                "Cross-task event firehose. Args: \
                 `since_event_id|limit|event_type_filter` \
                 (all optional). Returns one JSON object per \
                 line, newest-first by `event_id`. Each row \
                 carries `task_id` so consumers render without \
                 a second round-trip. Operators wire this into \
                 a global live tail.",
                &["task", "read", "events", "operator"],
            ),
            (
                "task.interruption_check",
                "Cooperative-poller snapshot of interruption \
                 state. Args: `task_id`. Returns the current \
                 status + pause_generation + freeze_generation. \
                 Runtime workers compare the returned \
                 generations against their cached value to \
                 detect a new operator pause/freeze request. \
                 HONEST: the alpha runtime does not yet poll \
                 this — it is the read side of the cooperative \
                 interruption protocol introduced in M70.",
                &["task", "read", "interrupt", "runtime"],
            ),
            (
                "task.observe_interruption",
                "Runtime ack that a cooperative worker noticed \
                 an interruption. Args: \
                 `task_id|pause|resume|freeze|generation`. \
                 Emits the matching `task.pause_observed` / \
                 `task.resume_observed` / `task.freeze_propagated` \
                 chronicle event with the observer subject_id + \
                 the generation observed. Distinguishes operator \
                 INTENT (the original request event) from \
                 runtime ACK — a request with no matching \
                 observation means the runtime never noticed.",
                &["task", "write", "interrupt", "runtime"],
            ),
            (
                "task.freeze",
                "Operator-initiated workflow freeze. Distinct \
                 from pause: freeze is intended to propagate \
                 down the spawned/delegated subtree once those \
                 edge producers ship. Status → `frozen`, bumps \
                 `freeze_generation`, emits \
                 `task.freeze_requested`. HONEST: today \
                 single-task scope; cooperative workers will \
                 observe + propagate via M70 protocol.",
                &["task", "write", "intervene", "operator"],
            ),
            (
                "task.unfreeze",
                "Operator-initiated unfreeze. Refuses any \
                 status other than `frozen`. Status → \
                 `pending`, clears `frozen_at` + \
                 `frozen_reason`, bumps `freeze_generation`, \
                 emits `task.unfreeze_requested` with the \
                 pre-freeze status recovered from the \
                 chronicle.",
                &["task", "write", "intervene", "operator"],
            ),
            (
                "task.record_spawned",
                "Attest a `spawned` cross-task edge. The \
                 caller (runtime worker, CLI, external \
                 orchestrator) declares it observed parent \
                 spawning child. Emits `task.spawned_child` \
                 chronicle event on the parent + inserts \
                 the edge with full producer/branch/context \
                 metadata. HONEST: no runtime path \
                 auto-emits today — the attestation API is \
                 ready for future producers.",
                &["task", "write", "graph", "lineage", "runtime"],
            ),
            (
                "task.record_delegated",
                "Attest a `delegated_to` cross-task edge. \
                 Parent passed completion responsibility to \
                 child rather than fanning out. Optional \
                 reason captured verbatim in payload_json.",
                &["task", "write", "graph", "lineage", "runtime"],
            ),
            (
                "task.record_awaited",
                "Attest an `awaited` cross-task edge. Parent \
                 is blocked waiting on the awaited task. \
                 Optional reason captured verbatim.",
                &["task", "write", "graph", "lineage", "runtime"],
            ),
            (
                "task.transition_check",
                "Informational state-machine validator. Args: \
                 `task_id|target_status`. Reads current \
                 status + returns `allowed=true|false` against \
                 the canonical transition matrix. Does NOT \
                 mutate. Operators + runtime workers use this \
                 to pre-flight a planned transition. The \
                 `task.update` path is not yet enforced \
                 against the matrix (separate milestone) — \
                 this is the honest authoritative reference.",
                &["task", "read", "state-machine"],
            ),
            (
                "task.subtree_metrics",
                "Aggregate runtime metrics over an execution \
                 subtree. Args: `task_id|max_depth` \
                 (max_depth defaults to 4, clamped to [1, 16]). \
                 Walks the M66 lineage + rolls up per-task \
                 status, attempt count, and wall-clock \
                 durations into a single k=v envelope. Pure \
                 read. Honest about missing timing — tasks \
                 with no started_at contribute zero to wall \
                 clock and increment tasks_with_missing_timing.",
                &["task", "read", "graph", "metrics"],
            ),
            (
                "task.stuck",
                "H6: stuck-running task projection. Arg: \
                 `<threshold_secs>` (default 300). Returns one \
                 tab-separated row per task that is `running`, \
                 has no max_runtime_secs, and has been running \
                 longer than the threshold (so the recovery scan \
                 cannot reach it). Output: \
                 `<task_id>\\t<title>\\t<started_at>\\t<age_secs>` \
                 + trailing `count=<N>`. Pure read; no side effects.",
                &["task", "read", "diagnostics"],
            ),
            (
                "task.todo_set",
                "PH-WAVE2D: replace the full per-task todo list. \
                 Arg: `<task_id>|<text1>\\n<text2>\\n...`. Each \
                 text is trimmed and scrubbed via the H8 redactor \
                 before persisting. Empty input clears the list. \
                 Returns the resulting list as tab-separated \
                 `<position>\\t<todo_id>\\t<status>\\t<text>` rows \
                 + trailing `count=<N>`.",
                &["task", "todo", "write"],
            ),
            (
                "task.todo_list",
                "PH-WAVE2D: read-only per-task todo list. Arg: \
                 `<task_id>`. Returns the same shape as \
                 task.todo_set. Empty list for tasks with no \
                 todos.",
                &["task", "todo", "read"],
            ),
            (
                "task.todo_update",
                "PH-WAVE2D: toggle a single todo's status. Arg: \
                 `<task_id>|<todo_id>|<open|done>`. Returns the \
                 updated row.",
                &["task", "todo", "write"],
            ),
            (
                "tool.browser.open_session",
                "CW4: open a browser session. Returns the \
                 session id. Today the \"none\" backend allocates \
                 ids without driving a real browser; navigate / \
                 screenshot return BackendNotConnected until a \
                 real backend lands. See docs/browser-tool.md.",
                &["browser", "session", "write"],
            ),
            (
                "tool.browser.navigate",
                "CW4: navigate a browser session. \
                 BackendNotConnected today.",
                &["browser", "navigation", "write"],
            ),
            (
                "tool.browser.list_sessions",
                "CW4: list open browser sessions.",
                &["browser", "read"],
            ),
            (
                "tool.mcp.list_servers",
                "CW5: list operator-declared MCP servers and \
                 their wire metadata. Honest: status=configured \
                 (not connected) until the live client lands.",
                &["mcp", "registry", "read"],
            ),
            (
                "tool.mcp.invoke",
                "CW5: invoke a tool on an MCP server. \
                 RuntimeNotConnected today; live client lands \
                 in a follow-up.",
                &["mcp", "execute", "write"],
            ),
            (
                "task.events",
                "Incremental chronicle fetch (task_id|after_id|limit). \
                 Returns one JSON event per line; empty when nothing is \
                 newer than after_id.",
                &["task", "read", "events"],
            ),
            (
                "task.recover",
                "Run the recovery scan now: promotes overdue running tasks to \
                 interrupted. Operator-only; idempotent.",
                &["task", "recover", "operator"],
            ),
            (
                "task.attempts",
                "Return the per-attempt timeline for one Task.",
                &["task", "read"],
            ),
            (
                "task.retry",
                "Operator-initiated retry: validates state + retry budget, flips \
                 status to retrying, emits task.retry_requested.",
                &["task", "retry", "operator"],
            ),
            (
                "task.replay",
                "W2-001b: clone a task into a brand-new replay. Args: <original_task_id>. \
                 New task inherits flow_template/params/retry-policy/origin_surface; \
                 retry_count starts at zero; a retried_from edge links the new task back \
                 to the original. Returns the new task_id.",
                &["task", "retry", "operator", "replay"],
            ),
        ];
        for (m, desc, cats) in coord_caps {
            // PH-CAP-RISK: coord task caps fall into two
            // operator-visible buckets — pure reads (`read` in
            // categories) are Safe, every other mutates
            // chronicle / task state in bounded ways, so Low.
            let risk = if cats.contains(&"read") {
                relix_core::capability::RiskLevel::Safe
            } else {
                relix_core::capability::RiskLevel::Low
            };
            manifest.add_capability(
                CapabilityDescriptor::unary(*m)
                    .with_description(*desc)
                    .with_categories(cats.iter().map(|s| (*s).into()))
                    .with_risk(risk),
            );
        }
        tracing::info!(
            db = %coord_cfg.db_path.display(),
            max_list = coord_cfg.max_list,
            recovery_scan = coord_cfg.recovery_scan,
            "coordinator node: registered task.create / update / event / get / list / count / list_cursor / events / recover / attempts / retry / export / compact_events / edges / note / mark_investigation / pause / resume / lineage / recent_events / interruption_check / observe_interruption / freeze / unfreeze / record_spawned / record_delegated / record_awaited / transition_check / subtree_metrics"
        );
    }
    if cfg.controller.node_type == "telegram" {
        let raw = cfg
            .telegram
            .clone()
            .ok_or_else(|| "node_type=telegram requires a [telegram] section".to_string())?;
        let tg_cfg: crate::nodes::telegram::TelegramNodeConfig = raw
            .try_into()
            .map_err(|e: toml::de::Error| format!("[telegram] parse: {e}"))?;
        tg_cfg
            .validate()
            .map_err(|e| format!("[telegram] validation: {e}"))?;
        // Resolve the token at startup so we fail loudly
        // when the env var is missing; the live client never
        // sees the raw token after this line.
        let token = tg_cfg
            .resolve_token()
            .map_err(|e| format!("[telegram] token: {e}"))?;
        let state = Arc::new(crate::nodes::telegram::ChannelState::default());
        let ring = Arc::new(crate::nodes::telegram::MessageRing::new(
            tg_cfg.messages_ring_capacity,
        ));
        let notifier = Arc::new(crate::nodes::telegram::NotifierState::default());
        let out_cell: crate::nodes::telegram::TelegramOutboundClientCell =
            Arc::new(tokio::sync::OnceCell::new());
        // Build the live bot API once and share it between the
        // long-poll loop AND the `telegram.send` capability.
        let api: Arc<dyn relix_telegram::BotApi> = Arc::new(relix_telegram::LiveBotApi::new(token));
        let cfg_arc = Arc::new(tg_cfg.clone());
        // FIX 1: register the inbound caps. When effective_mode
        // is Webhook, the new `telegram.webhook_update` cap is
        // wired alongside the existing read-only +
        // approval-send caps so the bridge can forward inbound
        // updates here.
        crate::nodes::telegram::register_with_webhook(
            bridge,
            state.clone(),
            ring.clone(),
            api.clone(),
            Some(out_cell.clone()),
            Some(cfg_arc.clone()),
        );
        let effective = tg_cfg.effective_mode();
        match effective {
            relix_telegram::config::DeliveryMode::Webhook => {
                // FIX 1: register the URL with Telegram once at
                // startup. A failure here logs ERROR but does
                // NOT crash the controller — the
                // `telegram.webhook_update` cap is still wired,
                // so an operator who manually calls setWebhook
                // out-of-band still gets inbound dispatch.
                let webhook_url = tg_cfg.webhook_url.clone().unwrap_or_default();
                if !webhook_url.trim().is_empty() {
                    let api_for_set = api.clone();
                    let url_for_set = webhook_url.clone();
                    tokio::spawn(async move {
                        match api_for_set.set_webhook(&url_for_set).await {
                            Ok(()) => tracing::info!(
                                url = %url_for_set,
                                "telegram: setWebhook registered (FIX 1)"
                            ),
                            Err(e) => tracing::error!(
                                error = %e,
                                url = %url_for_set,
                                "telegram: setWebhook failed; webhook receive path may not work \
                                 until operator manually registers the URL"
                            ),
                        }
                    });
                }
                tracing::info!("Telegram controller in WEBHOOK mode; long-poll loop suppressed");
                // Drop the notifier (the approval-notifier loop
                // is run independently if configured); the
                // long-poll loop is NOT spawned in webhook
                // mode.
                let _ = notifier;
            }
            relix_telegram::config::DeliveryMode::LongPoll => {
                // Spawn the long-poll loop. The loop checks the
                // out_cell on every tick and gracefully degrades
                // when the mesh client isn't wired yet (sends a
                // fallback reply rather than crashing).
                let api_for_loop = api.clone();
                let state_for_loop = state.clone();
                let ring_for_loop = ring.clone();
                let cfg_for_loop = cfg_arc.clone();
                let out_for_loop = out_cell.clone();
                tokio::spawn(async move {
                    crate::nodes::telegram::run_telegram_controller(
                        api_for_loop,
                        out_for_loop,
                        state_for_loop,
                        ring_for_loop,
                        notifier,
                        cfg_for_loop,
                    )
                    .await;
                });
            }
        }
        // Hand back to run() so the post-rpc::Client setup
        // can dial memory + ai + coord and publish the
        // outbound client into the cell.
        let tg_cfg_for_wiring = tg_cfg.clone();
        out.push(StartupWiring::Telegram {
            cell: out_cell,
            cfg: tg_cfg_for_wiring,
        });
        let telegram_caps: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "telegram.status",
                "Bot online status + username + own user_id. Read-only \
                 capability the bridge proxies for the dashboard.",
                &["read", "telegram", "status"],
                &["reads:internal"],
            ),
            (
                "telegram.messages_recent",
                "Last N inbound messages from the bounded in-memory ring \
                 (newest-first). Used by the dashboard's recent-messages \
                 widget.",
                &["read", "telegram", "messages"],
                &["reads:internal"],
            ),
            (
                "telegram.send",
                "Send a Telegram message from outside the long-poll loop. \
                 Args: JSON {chat_id (string, numeric), text}. Returns \
                 {ok:true} on success. Used by the alert fan-out + any \
                 coordinator code that needs to push a message.",
                &["write", "telegram", "send"],
                &["sends:external"],
            ),
            (
                "telegram.approval_send",
                "Render + send an approval request via the rich \
                 InlineKeyboardMarkup dispatcher. Args: JSON shape of \
                 ApprovalSendArgs (approval_id, agent_name, capability, \
                 request_summary, session_id, is_escalation, target_id). \
                 Returns {ok:true} on success. Routed via the coordinator's \
                 ApprovalDeliveryService.",
                &["write", "telegram", "approval"],
                &["sends:external"],
            ),
        ];
        for (method, doc, cats, sensitivities) in telegram_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            desc = desc.with_sensitivity(sensitivities.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }
        tracing::info!(
            allow_everyone = tg_cfg.allow_everyone(),
            operator_chat_id = tg_cfg.operator_chat_id,
            ring_capacity = tg_cfg.messages_ring_capacity,
            "telegram node: registered telegram.status / telegram.messages_recent; long-poll loop spawned"
        );
    }
    if cfg.controller.node_type == "discord" {
        let raw = cfg
            .discord
            .clone()
            .ok_or_else(|| "node_type=discord requires a [discord] section".to_string())?;
        let dc_cfg: crate::nodes::discord::DiscordNodeConfig = raw
            .try_into()
            .map_err(|e: toml::de::Error| format!("[discord] parse: {e}"))?;
        dc_cfg
            .validate()
            .map_err(|e| format!("[discord] validation: {e}"))?;
        let token = dc_cfg
            .resolve_token()
            .map_err(|e| format!("[discord] token: {e}"))?;
        let state = Arc::new(crate::nodes::discord::ChannelState::default());
        let ring = Arc::new(crate::nodes::discord::MessageRing::new(
            dc_cfg.messages_ring_capacity,
        ));
        let out_cell: crate::nodes::discord::DiscordOutboundClientCell =
            Arc::new(tokio::sync::OnceCell::new());
        let api: Arc<dyn relix_discord::DiscordApi> =
            Arc::new(relix_discord::LiveDiscordApi::new(token));
        crate::nodes::discord::register(
            bridge,
            state.clone(),
            ring.clone(),
            dc_cfg.channel_id.clone(),
            api.clone(),
        );
        let api_for_loop = api.clone();
        let state_for_loop = state.clone();
        let ring_for_loop = ring.clone();
        let cfg_for_loop = Arc::new(dc_cfg.clone());
        let out_for_loop = out_cell.clone();
        tokio::spawn(async move {
            crate::nodes::discord::run_discord_controller(
                api_for_loop,
                out_for_loop,
                state_for_loop,
                ring_for_loop,
                cfg_for_loop,
            )
            .await;
        });
        let dc_cfg_for_wiring = dc_cfg.clone();
        out.push(StartupWiring::Discord {
            cell: out_cell,
            cfg: dc_cfg_for_wiring,
        });
        let discord_caps: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "discord.status",
                "Bot online status + username + user_id + channel_id. \
                 Read-only capability the bridge proxies for the dashboard.",
                &["read", "discord", "status"],
                &["reads:internal"],
            ),
            (
                "discord.messages_recent",
                "Last N inbound messages from the bounded in-memory ring \
                 (newest-first). Used by the dashboard's recent-messages \
                 widget.",
                &["read", "discord", "messages"],
                &["reads:internal"],
            ),
            (
                "discord.send",
                "Send a Discord message from outside the inbound polling \
                 loop. Args: JSON {channel_id (snowflake string), text}. \
                 Returns {ok:true} on success. Used by the alert fan-out \
                 + any coordinator code that needs to push a message.",
                &["write", "discord", "send"],
                &["sends:external"],
            ),
            (
                "discord.approval_send",
                "Render + send an approval request via the rich \
                 component-button dispatcher. Args: JSON shape of \
                 ApprovalSendArgs (target_id = channel snowflake). Returns \
                 {ok:true} on success.",
                &["write", "discord", "approval"],
                &["sends:external"],
            ),
        ];
        for (method, doc, cats, sensitivities) in discord_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            desc = desc.with_sensitivity(sensitivities.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }
        tracing::info!(
            channel_id = %dc_cfg.channel_id,
            allow_everyone = dc_cfg.allow_everyone(),
            ring_capacity = dc_cfg.messages_ring_capacity,
            "discord node: registered discord.status / discord.messages_recent; polling loop spawned"
        );
    }
    if cfg.controller.node_type == "slack" {
        let raw = cfg
            .slack
            .clone()
            .ok_or_else(|| "node_type=slack requires a [slack] section".to_string())?;
        let sl_cfg: crate::nodes::slack::SlackNodeConfig = raw
            .try_into()
            .map_err(|e: toml::de::Error| format!("[slack] parse: {e}"))?;
        sl_cfg
            .validate()
            .map_err(|e| format!("[slack] validation: {e}"))?;
        let token = sl_cfg
            .resolve_token()
            .map_err(|e| format!("[slack] token: {e}"))?;
        let state = Arc::new(crate::nodes::slack::ChannelState::default());
        let ring = Arc::new(crate::nodes::slack::MessageRing::new(
            sl_cfg.messages_ring_capacity,
        ));
        let out_cell: crate::nodes::slack::SlackOutboundClientCell =
            Arc::new(tokio::sync::OnceCell::new());
        let api: Arc<dyn relix_slack::SlackApi> = Arc::new(relix_slack::LiveSlackApi::new(token));
        crate::nodes::slack::register(
            bridge,
            state.clone(),
            ring.clone(),
            sl_cfg.channel_id.clone(),
            api.clone(),
        );
        let api_for_loop = api.clone();
        let state_for_loop = state.clone();
        let ring_for_loop = ring.clone();
        let cfg_for_loop = Arc::new(sl_cfg.clone());
        let out_for_loop = out_cell.clone();
        tokio::spawn(async move {
            crate::nodes::slack::run_slack_controller(
                api_for_loop,
                out_for_loop,
                state_for_loop,
                ring_for_loop,
                cfg_for_loop,
            )
            .await;
        });
        let sl_cfg_for_wiring = sl_cfg.clone();
        out.push(StartupWiring::Slack {
            cell: out_cell,
            cfg: sl_cfg_for_wiring,
        });
        let slack_caps: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "slack.status",
                "Bot online status + username + user_id + team_id + channel_id. \
                 Read-only capability the bridge proxies for the dashboard.",
                &["read", "slack", "status"],
                &["reads:internal"],
            ),
            (
                "slack.messages_recent",
                "Last N inbound messages from the bounded in-memory ring \
                 (newest-first). Used by the dashboard's recent-messages \
                 widget.",
                &["read", "slack", "messages"],
                &["reads:internal"],
            ),
            (
                "slack.send",
                "Send a Slack message from outside the polling loop. \
                 Args: JSON {channel (id or #name), text}. Returns \
                 {ok:true} on success. Used by the alert fan-out + any \
                 coordinator code that needs to push a message.",
                &["write", "slack", "send"],
                &["sends:external"],
            ),
            (
                "slack.approval_send",
                "Render + send an approval request via the rich Block Kit \
                 dispatcher. Args: JSON shape of ApprovalSendArgs \
                 (target_id = channel id `C…`). Returns {ok:true} on success.",
                &["write", "slack", "approval"],
                &["sends:external"],
            ),
        ];
        for (method, doc, cats, sensitivities) in slack_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            desc = desc.with_sensitivity(sensitivities.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }
        tracing::info!(
            channel_id = %sl_cfg.channel_id,
            allow_everyone = sl_cfg.allow_everyone(),
            ring_capacity = sl_cfg.messages_ring_capacity,
            "slack node: registered slack.status / slack.messages_recent; polling loop spawned"
        );
    }
    if cfg.controller.node_type == "email" {
        let raw = cfg
            .email
            .clone()
            .ok_or_else(|| "node_type=email requires an [email] section".to_string())?;
        let em_cfg: crate::nodes::email::EmailNodeConfig = raw
            .try_into()
            .map_err(|e: toml::de::Error| format!("[email] parse: {e}"))?;
        em_cfg
            .validate()
            .map_err(|e| format!("[email] validation: {e}"))?;
        let smtp = std::sync::Arc::new(
            crate::nodes::email::SmtpSender::from_config(&em_cfg)
                .map_err(|e| format!("[email] smtp init: {e}"))?,
        );
        let state = Arc::new(crate::nodes::email::EmailChannelState::default());
        let ring = Arc::new(crate::nodes::email::MessageRing::new(
            em_cfg.messages_ring_capacity,
        ));
        let out_cell: crate::nodes::email::EmailOutboundClientCell =
            Arc::new(tokio::sync::OnceCell::new());
        crate::nodes::email::register(bridge, state.clone(), ring.clone(), smtp.clone());

        let em_cfg_arc = Arc::new(em_cfg.clone());
        let state_for_loop = state.clone();
        let ring_for_loop = ring.clone();
        let out_for_loop = out_cell.clone();
        let smtp_for_loop = smtp.clone();
        tokio::spawn(async move {
            crate::nodes::email::run_email_controller(
                em_cfg_arc,
                smtp_for_loop,
                out_for_loop,
                state_for_loop,
                ring_for_loop,
            )
            .await;
        });
        let em_cfg_for_wiring = em_cfg.clone();
        out.push(StartupWiring::Email {
            cell: out_cell,
            cfg: Box::new(em_cfg_for_wiring),
        });
        let email_caps: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "email.status",
                "SMTP + IMAP connection state, last successful send / poll timestamps, \
                 + counters. Read-only capability the bridge proxies for the dashboard.",
                &["read", "email", "status"],
                &["reads:internal"],
            ),
            (
                "email.messages_recent",
                "Last N inbound emails from the bounded in-memory ring (newest-first). \
                 Used by the dashboard's recent-messages widget.",
                &["read", "email", "messages"],
                &["reads:internal"],
            ),
            (
                "email.send",
                "Send an email via SMTP. Args: JSON { to, subject, body, html?, cc?, \
                 bcc?, reply_to?, in_reply_to?, references?, attachments? }. \
                 Returns { message_id }.",
                &["write", "email", "send"],
                &["sends:external"],
            ),
            (
                "email.send_template",
                "Render + send a templated email. Args: JSON { template_name, to, \
                 variables, cc?, bcc?, reply_to?, in_reply_to?, references? }. \
                 Templates resolve from `RELIX_EMAIL_TEMPLATES_DIR` or the built-in \
                 registry (welcome / reset_password / task_completed / task_failed).",
                &["write", "email", "send_template"],
                &["sends:external"],
            ),
            (
                "email.approval_send",
                "Render + send an approval email. Args: JSON shape of \
                 ApprovalSendArgs (target_id = recipient mailbox, \
                 target_extra = Reply-To address). Subject is \
                 `Approval Required: <cap> [<id>]` so the bridge's \
                 /v1/channels/email/reply route can route the operator's \
                 reply back to record_decision.",
                &["write", "email", "approval"],
                &["sends:external"],
            ),
        ];
        for (method, doc, cats, sensitivities) in email_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            desc = desc.with_sensitivity(sensitivities.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }
        tracing::info!(
            smtp_host = %em_cfg.smtp_host,
            imap_host = %em_cfg.imap_host,
            imap_folder = %em_cfg.imap_folder,
            allow_everyone = em_cfg.allow_everyone(),
            dkim = em_cfg.dkim_enabled(),
            ring_capacity = em_cfg.messages_ring_capacity,
            "email node: registered email.status / email.messages_recent / email.send / email.send_template; IMAP listener spawned"
        );
    }
    if cfg.controller.node_type == "plugin_host" {
        let raw = cfg
            .plugin_host
            .clone()
            .ok_or_else(|| "node_type=plugin_host requires a [plugin_host] section".to_string())?;
        let ph_cfg: crate::plugin::PluginHostConfig = raw
            .try_into()
            .map_err(|e: toml::de::Error| format!("[plugin_host] parse: {e}"))?;
        let registry_path = ph_cfg
            .registry_db_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("dev-data/plugin-registry.db"));
        let registry = Arc::new(
            crate::plugin::PluginRegistry::open(&registry_path)
                .map_err(|e| format!("[plugin_host] registry: {e}"))?,
        );
        let host_state = crate::plugin::PluginHostState::new(registry.clone());
        // Discover + load every plugin in plugin_dir. Each
        // successful load registers its capabilities on the
        // bridge as FnHandlers wrapping the per-plugin
        // dispatcher. Failures are surfaced via the registry
        // (status = "error", error_message set) so the dashboard
        // can show them.
        let manifests = crate::plugin::PluginLoader::find_manifests(&ph_cfg.plugin_dir)
            .map_err(|e| format!("[plugin_host] scan plugin_dir: {e}"))?;
        if manifests.len() > ph_cfg.max_plugins {
            tracing::warn!(
                found = manifests.len(),
                cap = ph_cfg.max_plugins,
                "plugin_host: more manifests than max_plugins cap; truncating"
            );
        }
        let host_handle = tokio::runtime::Handle::current();
        let plugins_to_load: Vec<_> = manifests.into_iter().take(ph_cfg.max_plugins).collect();
        for manifest_path in plugins_to_load {
            let plugin_manifest =
                match crate::plugin::PluginManifest::load_from_path(&manifest_path) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(
                            path = %manifest_path.display(),
                            error = %e,
                            "plugin_host: skipping invalid manifest"
                        );
                        continue;
                    }
                };
            let plugin_id = match registry.upsert(&plugin_manifest, &manifest_path) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        path = %manifest_path.display(),
                        error = %e,
                        "plugin_host: registry upsert failed; skipping"
                    );
                    continue;
                }
            };
            let manifest_for_spawn = plugin_manifest.clone();
            let manifest_path_for_spawn = manifest_path.clone();
            // Block on the spawn synchronously so the
            // controller startup sequence sees a fully-wired
            // bridge before run() unblocks. We're inside the
            // tokio runtime here, so use block_in_place so the
            // worker can drive the spawn future without
            // panicking on a nested block_on. 10s + 30s
            // timeouts.
            // SEC PART 2: thread the per-plugin sandbox caps
            // from [plugin_host] into the loader so the child
            // process is started under RLIMIT_AS / RLIMIT_CPU
            // (Unix) and the per-plugin bearer token wire.
            let sandbox_limits = crate::plugin::SandboxLimits {
                max_memory_mb: ph_cfg.max_memory_mb,
                max_cpu_secs: ph_cfg.max_cpu_secs,
                max_open_fds: 100,
            };
            let loaded = match tokio::task::block_in_place(|| {
                host_handle.block_on(crate::plugin::PluginLoader::spawn(
                    manifest_for_spawn,
                    manifest_path_for_spawn,
                    10,
                    30,
                    sandbox_limits,
                ))
            }) {
                Ok(p) => p,
                Err(e) => {
                    let msg = format!("{e}");
                    if let Err(re) = registry.set_status(
                        &plugin_id,
                        crate::plugin::PluginStatus::Error,
                        Some(&msg),
                    ) {
                        tracing::warn!(error = %re, "plugin_host: failed to record error status");
                    }
                    tracing::warn!(
                        plugin = %plugin_manifest.plugin.name,
                        error = %e,
                        "plugin_host: plugin failed to start; status=error"
                    );
                    continue;
                }
            };
            // Register each capability on the bridge. The
            // FnHandler captures the dispatcher and routes
            // every call to /invoke. Any plugin-level error
            // maps to the right ErrorEnvelope kind.
            //
            // The handler is registered under TWO method names:
            //   - the bare manifest name (e.g. "hello.greet") so
            //     `remote_call("plugin_host", "hello.greet", ...)`
            //     in SOL and direct libp2p ping continue to work,
            //   - the peer-prefixed alias ("plugin_host.<method>")
            //     so `.sflow` callers, whose wire_method always
            //     carries the peer prefix the user typed, can hit
            //     the same handler. The Arc is cloned, so the
            //     second registration costs only an Arc bump.
            for cap in &plugin_manifest.plugin.capabilities.provides {
                let method = cap.method.clone();
                let dispatcher = loaded.dispatcher.clone();
                let deadline_secs = plugin_manifest.plugin.runtime.invoke_timeout_secs as i64;
                let handler: Arc<dyn crate::dispatch::Handler> = Arc::new(
                    crate::dispatch::FnHandler(move |ctx: crate::dispatch::InvocationCtx| {
                        let dispatcher = dispatcher.clone();
                        let method = method.clone();
                        async move {
                            let args = String::from_utf8(ctx.args.clone())
                                .unwrap_or_else(|_| String::new());
                            let req = crate::plugin::InvokeRequest {
                                method: method.clone(),
                                args,
                                trace_id: format!("{}", ctx.trace_id),
                                request_id: format!("{}", ctx.request_id),
                                caller_subject_id: format!("{}", ctx.caller.subject_id),
                                deadline_unix: unix_now() + deadline_secs,
                            };
                            match dispatcher.invoke(req).await {
                                Ok(body) => crate::dispatch::HandlerOutcome::Ok(body.into_bytes()),
                                Err(crate::plugin::PluginInvokeError::Plugin { kind, cause }) => {
                                    crate::dispatch::HandlerOutcome::Err(
                                        relix_core::types::ErrorEnvelope {
                                            kind,
                                            cause: format!("{method}: {cause}"),
                                            retry_hint: 1,
                                            retry_after: None,
                                        },
                                    )
                                }
                                Err(e) => crate::dispatch::HandlerOutcome::Err(
                                    relix_core::types::ErrorEnvelope {
                                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                        cause: format!("{method}: {e}"),
                                        retry_hint: 1,
                                        retry_after: None,
                                    },
                                ),
                            }
                        }
                    }),
                );
                bridge.register(cap.method.clone(), handler.clone());
                bridge.register(format!("plugin_host.{}", cap.method), handler);
                // Advertise the plugin's capability on the
                // node's manifest so peers discover it. The
                // environment requirement tag carries the
                // plugin_id so operators can correlate
                // descriptors back to the manifest file.
                let risk = match cap.risk_level.as_str() {
                    "high" => relix_core::capability::RiskLevel::High,
                    "medium" => relix_core::capability::RiskLevel::Medium,
                    _ => relix_core::capability::RiskLevel::Low,
                };
                let mut node_desc =
                    CapabilityDescriptor::unary(&cap.method).with_description(&cap.description);
                node_desc = node_desc.with_categories(cap.categories.iter().cloned());
                node_desc = node_desc.with_sensitivity(cap.sensitivity_tags.iter().cloned());
                node_desc = node_desc.with_risk(risk);
                node_desc =
                    node_desc.with_environment_requirements([format!("plugin:{plugin_id}")]);
                manifest.add_capability(node_desc);
            }
            // Mark active + cache the loaded plugin.
            if let Err(e) =
                registry.set_status(&loaded.plugin_id, crate::plugin::PluginStatus::Active, None)
            {
                tracing::warn!(error = %e, "plugin_host: failed to flip status=active");
            }
            if let Err(e) = registry.touch(&loaded.plugin_id) {
                tracing::warn!(error = %e, "plugin_host: failed to touch last_seen_at");
            }
            tokio::task::block_in_place(|| {
                host_handle.block_on(async {
                    host_state
                        .plugins
                        .write()
                        .await
                        .insert(loaded.plugin_id.clone(), loaded.clone());
                });
            });
            tracing::info!(
                plugin = %plugin_manifest.plugin.name,
                plugin_id = %loaded.plugin_id,
                caps = ?plugin_manifest
                    .plugin
                    .capabilities
                    .provides
                    .iter()
                    .map(|c| c.method.as_str())
                    .collect::<Vec<_>>(),
                "plugin_host: plugin online"
            );
        }
        // Plugin management capabilities. Always registered,
        // even when no plugins are loaded — operators get a
        // consistent surface.
        register_plugin_management_capabilities(bridge, host_state.clone());
        let mgmt_caps: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "plugin.list",
                "List every plugin known to this plugin_host. \
                 Tab-separated rows + trailing count.",
                &["read", "plugin", "management"],
                &["reads:internal"],
            ),
            (
                "plugin.status",
                "Read one plugin's status by plugin_id. \
                 Returns pipe-delimited key=value fields.",
                &["read", "plugin", "management"],
                &["reads:internal"],
            ),
            (
                "plugin.reload",
                "Stop and restart one plugin's subprocess. \
                 Arg: plugin_id. Returns ok\\n.",
                &["mutate", "plugin", "management"],
                &["mutate:plugin", "external:subprocess"],
            ),
            (
                "plugin.disable",
                "Disable one plugin — flip status to disabled and \
                 kill the subprocess. Arg: plugin_id.",
                &["mutate", "plugin", "management"],
                &["mutate:plugin", "external:subprocess"],
            ),
        ];
        for (method, doc, cats, sens) in mgmt_caps {
            let mut desc = CapabilityDescriptor::unary(*method).with_description(*doc);
            desc = desc.with_categories(cats.iter().map(|s| (*s).into()));
            desc = desc.with_sensitivity(sens.iter().map(|s| (*s).into()));
            manifest.add_capability(desc);
        }
        let plugin_count = tokio::task::block_in_place(|| {
            host_handle.block_on(async { host_state.plugins.read().await.len() })
        });
        tracing::info!(
            plugin_dir = %ph_cfg.plugin_dir.display(),
            plugins_loaded = plugin_count,
            "plugin_host: registered plugin.list / status / reload / disable"
        );
    }
    if cfg.controller.node_type == "tool" {
        let tool_cfg: crate::nodes::tool::ToolConfig = match &cfg.tool {
            Some(raw) => raw
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| format!("[tool] parse: {e}"))?,
            None => crate::nodes::tool::ToolConfig::default(),
        };
        // SEC PART 6: install the process-global SSRF state so
        // every cloud-tier HTTP client (Tavily/Brave/Perplexity/
        // LlamaParse/Jina/Firecrawl) + every tool capability
        // handler (web_read/web_get/web_fetch/browser.*) calls
        // through the same check. `ssrf_protection = false`
        // logs a startup warning per the spec.
        crate::nodes::tool::security::install_ssrf_state(
            tool_cfg.ssrf_protection,
            crate::nodes::tool::security::UrlAllowlist::new(tool_cfg.url_allowlist.iter()),
            300,
        );
        let backend = std::sync::Arc::new(crate::nodes::tool::ToolBackend::new(tool_cfg.clone())?);
        // W3: operator channel for tool.ask_human. Allocated as
        // an empty OnceCell; future controller-side wiring
        // populates it with the configured channel (Telegram
        // approval queue, dashboard intervention). When empty
        // the ask_human handler returns `{"timeout": true}`
        // honest-to-the-fact-that-no-operator-is-available.
        let operator_channel: crate::nodes::tool::ask_human::OperatorChannelHandle =
            std::sync::Arc::new(tokio::sync::OnceCell::new());
        // Session-search proxy cell. Empty by default — the
        // capability is registered + advertised, but the
        // handler returns PEER_UNREACHABLE until a future
        // [tool.memory_peer] config block populates the cell
        // (parity with how the AI node dials its memory peer).
        let session_search_handle: crate::nodes::tool::session_search_proxy::MemorySessionSearchProxyHandle =
            std::sync::Arc::new(tokio::sync::OnceCell::new());
        crate::nodes::tool::register(bridge, backend, operator_channel, session_search_handle);
        // SEC §14: advertise EXACTLY the capability set returned by
        // `crate::nodes::tool::advertised_capabilities` — the single
        // source of truth the policy-coverage contract test diffs
        // `configs/policies/tool.toml` against. A new tool capability
        // added there without a policy rule fails that test.
        for cap in crate::nodes::tool::advertised_capabilities(&tool_cfg) {
            manifest.add_capability(cap);
        }
        tracing::info!(
            max_bytes = tool_cfg.max_bytes,
            timeout_secs = tool_cfg.timeout_secs,
            max_redirects = tool_cfg.max_redirects,
            allow_http = tool_cfg.allow_http,
            cw3 = "tool.web_get, tool.web_search",
            "tool node: registered tool.web_fetch + CW3 web_tools"
        );
    }
    // SEC §13: unreachable for unsupported node_types — they were
    // rejected by `validate_controller_node_type` at the top of
    // this function rather than falling through to a no-op boot.
    Ok(())
}

/// Hook for node-specific modules to register their capabilities. Called by
/// node-type entry points if added in a future revision; the current controller
/// binary registers built-ins only.
#[allow(dead_code)]
pub fn extend_with_handler(
    _bridge: &mut DispatchBridge,
    _method: &str,
    _handler: Arc<dyn Handler>,
) {
    // Placeholder for the next milestone — keeps the public surface visible.
}

fn load_or_generate_key(path: &Path) -> Result<SigningKey, Box<dyn std::error::Error>> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        if bytes.len() != 32 {
            return Err(format!(
                "{}: expected 32-byte secret key, got {}",
                path.display(),
                bytes.len()
            )
            .into());
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(SigningKey::from_bytes(&arr))
    } else {
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, key.to_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(path)?.permissions();
            p.set_mode(0o600);
            std::fs::set_permissions(path, p)?;
        }
        tracing::info!(path = %path.display(), "generated new node identity key");
        Ok(key)
    }
}

fn load_pubkey(path: &Path) -> Result<VerifyingKey, Box<dyn std::error::Error>> {
    // Trust-root file MUST be a 32-byte Ed25519 PUBLIC key. The companion
    // `.pub` file emitted by `relix-cli identity init-org` is the source of
    // truth. We deliberately do NOT accept a secret-key file here — silently
    // treating arbitrary 32 bytes as a pubkey was a real bug.
    let bytes = std::fs::read(path)?;
    if bytes.len() != 32 {
        return Err(format!(
            "{}: expected 32-byte Ed25519 public key, got {}",
            path.display(),
            bytes.len()
        )
        .into());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(VerifyingKey::from_bytes(&arr)?)
}

fn data_dir_for(node_name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = std::env::var("RELIX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".relix")
        });
    Ok(base.join(node_name))
}

// Re-export for the controller binary main.
pub use crate::transport::rpc::Client as TransportClient;

// Channel type needed by some downstream uses; suppress unused-warning otherwise.
#[allow(dead_code)]
type _UnusedReceiver = mpsc::Receiver<TransportEvent>;

/// Background retention loop. Sleeps for `compact_interval_h`
/// hours, then runs one bounded compact pass against the
/// configured cutoff. Failures are logged but never propagated
/// — the loop continues so a transient SQLite hiccup doesn't
/// silently disable retention until restart. See
/// `docs/chronicle-retention.md`.
async fn run_retention_loop(
    store: std::sync::Arc<crate::nodes::coordinator::TaskStore>,
    cfg: crate::nodes::coordinator::RetentionConfig,
) {
    use std::time::Duration;
    let interval = Duration::from_secs(u64::from(cfg.compact_interval_h.max(1)) * 3600);
    // Initial delay so retention doesn't run immediately at
    // startup — gives the node time to admit traffic and
    // confirm health before any deletion happens. One full
    // interval is the safest choice.
    tokio::time::sleep(interval).await;
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let cutoff_ts = now - (i64::from(cfg.max_task_age_days) * 86_400);
        let store_for_run = store.clone();
        let max_passes = cfg.max_passes_per_run;
        // Move the synchronous-SQLite work onto a blocking
        // thread so the tokio runtime's IO threads aren't
        // pinned by the bounded-delete loop. A single
        // retention run can stretch across several seconds on
        // a large DB; that's fine on a blocking thread.
        let result =
            tokio::task::spawn_blocking(move || store_for_run.run_retention(cutoff_ts, max_passes))
                .await;
        match result {
            Ok(Ok(r)) => {
                if r.events_deleted > 0 || r.snapshots_emitted > 0 {
                    tracing::info!(
                        events_deleted = r.events_deleted,
                        snapshots_emitted = r.snapshots_emitted,
                        tasks_compacted = r.tasks_compacted,
                        passes_run = r.passes_run,
                        stopped_at_pass_limit = r.stopped_at_pass_limit,
                        "coordinator: chronicle retention pass complete"
                    );
                } else {
                    tracing::debug!(
                        "coordinator: chronicle retention pass found nothing to compact"
                    );
                }
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "coordinator: retention pass failed");
            }
            Err(e) => {
                tracing::error!(error = %e, "coordinator: retention task panicked");
            }
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod confidence_wiring_tests {
    //! RELIX-7.19 GAP 4: confidence bundle wiring tests.
    //!
    //! Verifies `build_confidence_bundle_from` honours the
    //! `[confidence]` section + `enabled` switch, and that
    //! the returned bundle drops into the existing dispatch
    //! bridge cleanly.

    use super::*;
    use crate::confidence::ConfidenceConfig;

    #[test]
    fn build_confidence_bundle_returns_none_when_section_absent() {
        let b = build_confidence_bundle_from(None).expect("ok");
        assert!(b.is_none());
    }

    #[test]
    fn build_confidence_bundle_returns_none_when_enabled_false() {
        let conf = ConfidenceConfig {
            enabled: false,
            ..Default::default()
        };
        let b = build_confidence_bundle_from(Some(&conf)).expect("ok");
        assert!(b.is_none());
    }

    #[test]
    fn build_confidence_bundle_returns_some_when_enabled_true() {
        let conf = ConfidenceConfig {
            enabled: true,
            ..Default::default()
        };
        let b = build_confidence_bundle_from(Some(&conf))
            .expect("ok")
            .expect("bundle");
        // Cell starts at neutral 1.0 per spec.
        assert!((b.cell.get() - 1.0).abs() < 1e-6);
        // Scorer can score — confirms the bundle is alive.
        let inputs = crate::confidence::ScoringInputs {
            response_body: b"a complete answer.",
            finish_reason: Some("stop"),
            logprob: None,
            latency_ms: 50,
            success: true,
            self_consistency: None,
        };
        let v = b.scorer.score("alice", "ai.chat", &inputs);
        assert!(v.final_score > 0.0);
    }

    #[test]
    fn confidence_bundle_drops_into_dispatch_bridge() {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        use relix_core::policy::PolicyEngine;
        use tempfile::TempDir;
        let conf = ConfidenceConfig {
            enabled: true,
            ..Default::default()
        };
        let bundle = build_confidence_bundle_from(Some(&conf)).unwrap().unwrap();
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let mut bridge = crate::dispatch::DispatchBridge::new(
            PolicyEngine::permissive(),
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        bridge.set_confidence(bundle.scorer.clone(), bundle.engine.clone());
        bridge.set_last_confidence_cell(bundle.cell.clone());
        assert!(bridge.confidence_scorer_handle().is_some());
        assert!(bridge.last_confidence_cell().is_some());
        // The cell handed back is the SAME cell — mutate via
        // the bundle, observe via the bridge handle.
        bundle.cell.set(0.42);
        assert!(
            (bridge.last_confidence_cell().unwrap().get() - 0.42).abs() < 1e-6,
            "cell is shared storage"
        );
    }

    #[test]
    fn shared_cell_between_bundle_and_bridge_round_trips_through_dispatch() {
        // RELIX-7.19 GAP 4 invariant: the cell handed to the
        // bridge IS the same cell the SOL VM reads from.
        let conf = ConfidenceConfig {
            enabled: true,
            ..Default::default()
        };
        let bundle = build_confidence_bundle_from(Some(&conf)).unwrap().unwrap();
        // Imitate flow_runner: hand the cell to the VM.
        let mut vm = crate::sol::vm::VM::new();
        vm.set_last_confidence_cell(bundle.cell.clone());
        bundle.cell.set(0.73);
        assert!((vm.last_confidence() - 0.73).abs() < 1e-6);
    }
}

/// GATE 1 fail-closed decision for `verify_on_dispatch`.
///
/// Given whether the operator requested `verify_on_dispatch` and
/// whether the session verification service actually wired up,
/// decide whether boot may proceed. Returns `Err(diagnostic)`
/// when the gate was requested but cannot be enforced — the
/// caller (controller boot) propagates the error and REFUSES TO
/// BOOT rather than admitting capability calls unverified.
///
/// Behaviour is unchanged when `verify_on_dispatch` is false:
/// the function returns `Ok(())` regardless of service state.
///
/// Factored out of the boot path so the decision is unit-
/// testable without standing up the whole controller.
fn session_verification_boot_gate(
    verify_requested: bool,
    service_wired: bool,
    section_present: bool,
    enabled: bool,
    signing_key_env: &str,
    signing_key_len: usize,
) -> Result<(), String> {
    // Gate not requested, or it IS requested and the service is
    // wired: nothing to fail closed on.
    if !verify_requested || service_wired {
        return Ok(());
    }
    // Requested but unenforceable — build a specific diagnostic
    // telling the operator exactly what is missing.
    let reason = if !section_present {
        "the [identity.session] config section is absent".to_string()
    } else if !enabled {
        "[identity.session] enabled = false — set it to true".to_string()
    } else if signing_key_len < 32 {
        format!(
            "the signing key env var `{signing_key_env}` is {signing_key_len} bytes; \
             at least 32 are required"
        )
    } else {
        "the session token store could not be opened or the service failed to construct \
         (see the WARN logs above)"
            .to_string()
    };
    Err(format!(
        "SECURITY: [identity.session] verify_on_dispatch = true but the session \
         verification service is not available, so capability calls would be admitted \
         WITHOUT session-token verification. Refusing to boot. Cause: {reason}."
    ))
}

#[cfg(test)]
mod gate1_boot_failclosed_tests {
    use super::session_verification_boot_gate;

    #[test]
    fn fails_closed_when_verify_requested_but_section_absent() {
        let err = session_verification_boot_gate(true, false, false, false, "", 0)
            .expect_err("must refuse to boot");
        assert!(err.contains("Refusing to boot"), "err: {err}");
        assert!(err.contains("config section is absent"), "err: {err}");
    }

    #[test]
    fn fails_closed_when_verify_requested_but_session_disabled() {
        let err = session_verification_boot_gate(
            true,
            false,
            true,
            false,
            "RELIX_SESSION_SIGNING_KEY",
            64,
        )
        .expect_err("must refuse to boot");
        assert!(err.contains("enabled = false"), "err: {err}");
    }

    #[test]
    fn fails_closed_when_verify_requested_but_key_too_short() {
        let err = session_verification_boot_gate(
            true,
            false,
            true,
            true,
            "RELIX_SESSION_SIGNING_KEY",
            12,
        )
        .expect_err("must refuse to boot");
        // The diagnostic must name the env var AND the length.
        assert!(err.contains("RELIX_SESSION_SIGNING_KEY"), "err: {err}");
        assert!(err.contains("12 bytes"), "err: {err}");
        assert!(err.contains("at least 32"), "err: {err}");
    }

    #[test]
    fn boots_when_verify_requested_and_service_wired() {
        // The fix must NOT break the gate the other way: when the
        // service IS wired, boot proceeds.
        assert!(
            session_verification_boot_gate(true, true, true, true, "RELIX_SESSION_SIGNING_KEY", 64)
                .is_ok()
        );
    }

    #[test]
    fn boots_unchanged_when_verify_not_requested() {
        // verify_on_dispatch = false → behaviour unchanged
        // regardless of whether a service is wired.
        assert!(session_verification_boot_gate(false, false, false, false, "", 0).is_ok());
        assert!(session_verification_boot_gate(false, true, true, true, "X", 64).is_ok());
    }
}

#[cfg(test)]
mod sec13_node_type_failclosed_tests {
    //! SEC §13: the controller must hard-error on a no-op / unknown
    //! node_type instead of booting a dead "online" process.
    use super::{SUPPORTED_CONTROLLER_NODE_TYPES, validate_controller_node_type};

    #[test]
    fn rejects_noop_and_unknown_node_types() {
        // The named offenders (web_bridge, demo), a typo, and an
        // empty node_type all fail closed with a clear message.
        for nt in ["web_bridge", "demo", "totally-unknown", ""] {
            let err = validate_controller_node_type(nt)
                .expect_err(&format!("node_type `{nt}` must be rejected"));
            assert!(
                err.contains("not implemented") && err.contains("dead process"),
                "error must explain the refusal for `{nt}`: {err}"
            );
            // The message must point operators at the supported set.
            assert!(
                err.contains("memory"),
                "error should list supported types: {err}"
            );
        }
    }

    #[test]
    fn real_node_types_still_boot() {
        // Every implemented node_type passes validation (boots
        // normally) — the gate doesn't overshoot.
        for nt in SUPPORTED_CONTROLLER_NODE_TYPES {
            assert!(
                validate_controller_node_type(nt).is_ok(),
                "supported node_type `{nt}` must pass validation"
            );
        }
        // Sanity: the named real type from the section criteria.
        assert!(validate_controller_node_type("ai").is_ok());
        assert!(validate_controller_node_type("memory").is_ok());
    }
}

#[cfg(test)]
mod run_apply_capability_tests {
    //! In-process integration coverage for the `run.diff` / `run.apply`
    //! capability bodies, driven by a REAL run that a real adapter process
    //! actually writes files in — the deterministic equivalent of the live
    //! HTTP smoke (no paid/interactive CLI, no fake product adapter: just a
    //! `cmd`/`sh` one-liner registered as a test Rig). It proves the full
    //! spine contract through the SAME code the bridge route runs:
    //!   real changed file → diff preview (review-gated) → accept → apply into
    //!   the configured project root → productized review-to-done (the Brief
    //!   reaches board `done`, dependents unblock) → idempotent re-apply;
    //! and that a post-baseline divergence is detected as a conflict that
    //! refuses the whole apply and leaves the Brief in review.
    use super::{execute_run_apply, execute_run_diff};
    use crate::nodes::coordinator::heartbeat::{
        DEFAULT_WORKSPACE_MAX_BYTES, DEFAULT_WORKSPACE_MAX_FILES, WorkspaceConfig,
        WorkspaceContext, run_brief_now,
    };
    use crate::nodes::coordinator::{RetryPolicy, TaskStore};

    /// A store whose scoped workspaces land under `<workspace_root>/runs` and
    /// whose `copy_repo` snapshots come from `project_root` (a DISJOINT dir, so
    /// a copy can never recurse into the run tree). This is the operator config
    /// the Brief prompt never influences.
    fn store_with_project_root(
        workspace_root: &std::path::Path,
        project_root: &std::path::Path,
    ) -> TaskStore {
        let mut s = TaskStore::in_memory().unwrap();
        s.set_run_workspace_root(workspace_root.join("runs"));
        s.set_run_workspace_config(WorkspaceConfig {
            context: WorkspaceContext::CopyRepo,
            project_root: project_root.to_path_buf(),
            max_bytes: DEFAULT_WORKSPACE_MAX_BYTES,
            max_files: DEFAULT_WORKSPACE_MAX_FILES,
        });
        s
    }

    fn ready_brief(s: &TaskStore, title: &str, assignee: &str) -> String {
        let id = s
            .create(
                title,
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        s.set_brief_field(&id, "assignee", assignee).unwrap();
        s.set_brief_field(&id, "reviewer", "reviewer_1").unwrap();
        s.set_board_status(&id, "todo").unwrap();
        id
    }

    /// A real adapter that OVERWRITES `seed.txt` in its working directory (the
    /// scoped workspace) with `content`. Because `seed.txt` was copied in from
    /// the project root, this exercises the MODIFIED-file path (baseline hash →
    /// overwrite / conflict), the strongest apply semantics, not just create.
    fn seed_modifying_rig(content: &str) -> crate::rig::RigRegistry {
        let (prog, args) = if cfg!(windows) {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), format!("echo {content}> seed.txt")],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), format!("printf '{content}' > seed.txt")],
            )
        };
        let mut reg = crate::rig::RigRegistry::new();
        reg.register(std::sync::Arc::new(crate::rig::ProcessRig::new(
            "mk", prog, args,
        )));
        reg.set_default(Some("mk".to_string()));
        reg
    }

    #[test]
    fn run_apply_capability_proves_real_file_change_review_gate_and_review_to_done() {
        let ws_tmp = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        // A pre-existing project file the run will modify.
        std::fs::write(proj.path().join("seed.txt"), "v1").unwrap();
        let s = store_with_project_root(ws_tmp.path(), proj.path());

        // A track Brief plus a dependent that is blocked until the track is done.
        let track = ready_brief(&s, "edit the seed file", "agt_eng");
        let integrate = ready_brief(&s, "integrate", "agt_eng");
        s.add_snag(&integrate, &track).unwrap();

        // Run on a real adapter that rewrites seed.txt in the scoped workspace.
        let report = run_brief_now(
            &s,
            &seed_modifying_rig("v2"),
            None,
            300,
            &track,
            None,
            "go".into(),
        )
        .unwrap();
        let run_id = report.run_id.expect("a committed run has an id");

        // (1) The run captured a REAL changed file as an artifact, with the
        //     pre-run baseline hash safe-apply needs to detect divergence.
        let arts = s.list_run_artifacts(&run_id).unwrap();
        let seed_art = arts
            .iter()
            .find(|a| a.rel_path == "seed.txt")
            .expect("the seed.txt change is recorded as an artifact");
        assert_eq!(seed_art.kind, "modified");
        assert!(
            seed_art.baseline_hash.is_some(),
            "a modified artifact carries the pre-run baseline hash"
        );

        // The successful Shift parked the Brief in review; the dependent blocks.
        assert_eq!(
            s.board_status(&track).unwrap().as_deref(),
            Some("in_review")
        );
        assert!(
            s.is_blocked(&integrate).unwrap(),
            "integrate is blocked while the track awaits review"
        );

        // (2) Before review: run.diff shows the pending change but is INELIGIBLE.
        let diff = execute_run_diff(&s, &run_id, "default").unwrap();
        assert_eq!(diff["eligible"], serde_json::json!(false));
        assert!(
            diff["plan"]["changes"].as_u64().unwrap() >= 1,
            "the pending file change is previewable before acceptance"
        );
        // Apply is gated behind acceptance — nothing is written, Brief unmoved.
        assert!(
            execute_run_apply(&s, &run_id, "default").is_err(),
            "apply is refused until the run is accepted"
        );
        assert_eq!(
            std::fs::read_to_string(proj.path().join("seed.txt")).unwrap(),
            "v1",
            "a refused apply writes nothing"
        );
        assert_eq!(
            s.board_status(&track).unwrap().as_deref(),
            Some("in_review")
        );

        // (3) Accept → run.diff flips to eligible.
        s.set_run_review(&run_id, "accepted", "lgtm").unwrap();
        let diff2 = execute_run_diff(&s, &run_id, "default").unwrap();
        assert_eq!(diff2["eligible"], serde_json::json!(true));

        // (4/5) Apply writes the real change into the project root AND closes
        //       the review-to-done: the Brief reaches board `done`.
        let applied = execute_run_apply(&s, &run_id, "default").unwrap();
        assert_eq!(applied["apply_status"], serde_json::json!("applied"));
        assert!(applied["applied_files"].as_u64().unwrap() >= 1);
        assert_eq!(
            applied["brief_status"],
            serde_json::json!("done"),
            "a clean apply IS the operator's review-to-done"
        );
        let landed = std::fs::read_to_string(proj.path().join("seed.txt")).unwrap();
        assert!(
            landed.starts_with("v2"),
            "the run's real change must land in the project root: {landed:?}"
        );
        // Board done + the dependent unblocks — through the capability's code.
        assert_eq!(s.board_status(&track).unwrap().as_deref(), Some("done"));
        assert!(
            !s.is_blocked(&integrate).unwrap(),
            "with the track done, the dependent integrate Brief unblocks"
        );

        // (7) Idempotent re-apply: the target already matches → 0 writes, no
        //     corruption, no duplicate write.
        let again = execute_run_apply(&s, &run_id, "default").unwrap();
        assert_eq!(again["apply_status"], serde_json::json!("applied"));
        assert_eq!(
            again["applied_files"],
            serde_json::json!(0),
            "a second apply rewrites nothing"
        );
        assert_eq!(
            std::fs::read_to_string(proj.path().join("seed.txt")).unwrap(),
            landed,
            "idempotent re-apply must not corrupt the file"
        );
    }

    #[test]
    fn run_apply_capability_refuses_a_post_baseline_conflict_and_keeps_brief_in_review() {
        let ws_tmp = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("seed.txt"), "v1").unwrap();
        let s = store_with_project_root(ws_tmp.path(), proj.path());
        let track = ready_brief(&s, "edit the seed file", "agt_eng");
        let report = run_brief_now(
            &s,
            &seed_modifying_rig("v2"),
            None,
            300,
            &track,
            None,
            "go".into(),
        )
        .unwrap();
        let run_id = report.run_id.unwrap();
        s.set_run_review(&run_id, "accepted", "").unwrap();

        // The project file diverges from the run's baseline AFTER the run — a
        // real conflict the all-or-nothing apply must refuse (no three-way merge).
        std::fs::write(
            proj.path().join("seed.txt"),
            "operator edited this meanwhile",
        )
        .unwrap();

        let res = execute_run_apply(&s, &run_id, "default").unwrap();
        assert_eq!(res["apply_status"], serde_json::json!("conflicted"));
        assert_eq!(res["applied_files"], serde_json::json!(0));
        assert!(
            res["brief_status"].is_null(),
            "a conflicted apply does NOT review-to-done the Brief"
        );
        // The operator's intervening edit is untouched; the Brief stays in review.
        assert_eq!(
            std::fs::read_to_string(proj.path().join("seed.txt")).unwrap(),
            "operator edited this meanwhile"
        );
        assert_eq!(
            s.board_status(&track).unwrap().as_deref(),
            Some("in_review")
        );
        // The durable run row records the conflict.
        assert_eq!(
            s.get_run(&run_id).unwrap().unwrap().apply_status.as_deref(),
            Some("conflicted")
        );
    }

    #[test]
    fn run_apply_capability_is_tenant_scoped_and_gates_unaccepted_runs() {
        let ws_tmp = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("seed.txt"), "v1").unwrap();
        let s = store_with_project_root(ws_tmp.path(), proj.path());
        let track = ready_brief(&s, "edit", "agt_eng");
        s.set_task_tenant(&track, "guild-a").unwrap();
        let report = run_brief_now(
            &s,
            &seed_modifying_rig("v2"),
            None,
            300,
            &track,
            None,
            "go".into(),
        )
        .unwrap();
        let run_id = report.run_id.unwrap();

        // Another Guild cannot diff or apply this run — it reads as not-found.
        assert!(execute_run_diff(&s, &run_id, "guild-b").is_err());
        assert!(execute_run_apply(&s, &run_id, "guild-b").is_err());

        // The owning Guild can diff, but apply is refused until acceptance —
        // and the refusal records a durable `blocked` apply status, writing
        // nothing into the project root.
        assert!(execute_run_diff(&s, &run_id, "guild-a").is_ok());
        let err = execute_run_apply(&s, &run_id, "guild-a").unwrap_err();
        assert!(
            err.contains("apply refused"),
            "an unaccepted run is gated: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(proj.path().join("seed.txt")).unwrap(),
            "v1"
        );
        assert_eq!(
            s.get_run(&run_id).unwrap().unwrap().apply_status.as_deref(),
            Some("blocked")
        );
    }
}
