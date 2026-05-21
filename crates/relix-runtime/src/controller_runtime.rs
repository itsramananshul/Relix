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
    /// `[peers]` — alias → endpoint info.
    #[serde(default)]
    pub peers: std::collections::BTreeMap<String, PeerConfig>,
    /// SOL session declarations (M6).
    #[serde(default)]
    #[allow(dead_code)]
    pub session: std::collections::BTreeMap<String, SessionConfig>,
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
}

/// One peer alias.
#[derive(Clone, Debug, Deserialize)]
pub struct PeerConfig {
    /// libp2p TCP port to dial.
    pub port: u16,
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
    let manifest = ManifestProvider::new(
        node_id,
        cfg.controller.name.clone(),
        cfg.controller.node_type.clone(),
        NodeId::from_pubkey(trust_root.as_bytes()),
        vec![format!("/ip4/127.0.0.1/tcp/{}", cfg.controller.listen_port)],
    );

    // Dispatch bridge.
    let mut bridge = DispatchBridge::new(policy, trust_root, &audit_path, node_signer.clone())?;
    register_builtins(&mut bridge, &cfg, manifest.clone());
    // Router role short-circuits: it doesn't run the per-node-type
    // capability surface (memory/ai/tool/...) — it runs the four
    // router.* capabilities and the reaper background loops.
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
        register_node_type_handlers(&mut bridge, &cfg, manifest.clone())?;
        None
    };

    let bridge = Arc::new(bridge);

    // Transport.
    let (client, mut events, event_loop) =
        rpc::new(node_signer.to_bytes(), cfg.controller.listen_port).await?;
    tokio::spawn(event_loop.run());

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

    tracing::info!("controller online; awaiting inbound RPCs");

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
                tokio::spawn(async move {
                    let resp = bridge.handle_inbound(envelope).await;
                    respond.respond(resp).await;
                    let _ = from; // peer id available for future audit metadata
                });
            }
            TransportEvent::PeerConnected { peer_id, address } => {
                tracing::info!(peer = %peer_id, addr = %address, "peer connected");
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
    // Built-in: every node serves its own NodeManifest.
    let manifest_for_handler = manifest.clone();
    bridge.register(
        "node.manifest",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let provider = manifest_for_handler.clone();
            async move {
                let snap = provider.snapshot();
                match codec::encode(&snap) {
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
            .with_categories(["health".into()]),
    );
    manifest.add_capability(
        relix_core::capability::CapabilityDescriptor::unary("node.manifest")
            .with_description("Return this node's manifest (capability list + node identity).")
            .with_categories(["discover".into()]),
    );
}

/// Register node-type-specific capabilities based on `[controller] node_type`.
///
/// Advertise the four router.* capabilities in the manifest.
/// Called from `run()` only when `[controller] role = "router"`.
fn register_router_descriptors(manifest: &ManifestProvider) {
    use relix_core::capability::CapabilityDescriptor;
    manifest.add_capability(
        CapabilityDescriptor::unary("router.heartbeat")
            .with_description(
                "Controller-only: register or refresh this peer's liveness + capability list.",
            )
            .with_categories(["router".into(), "health".into()]),
    );
    manifest.add_capability(
        CapabilityDescriptor::unary("router.network_summary")
            .with_description(
                "Operator-facing mesh overview: known peers, active sessions, uptime.",
            )
            .with_categories(["router".into(), "observability".into()]),
    );
    manifest.add_capability(
        CapabilityDescriptor::unary("router.session_list")
            .with_description(
                "Operator-facing session browser. Supports status filter + pagination.",
            )
            .with_categories(["router".into(), "observability".into()]),
    );
    manifest.add_capability(
        CapabilityDescriptor::unary("router.log")
            .with_description(
                "Controller-only: push a structured log line to the router for aggregation.",
            )
            .with_categories(["router".into(), "observability".into()]),
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

/// - `memory` → SQLite + FTS5 memory store (M7).
/// - Other types (`ai`, `tool`, `web_bridge`, `demo`, ...) are no-ops until
///   their handlers ship in later milestones; the controller still serves
///   the default `node.health` capability so it can participate in chained
///   orchestration today.
fn register_node_type_handlers(
    bridge: &mut DispatchBridge,
    cfg: &ControllerConfig,
    manifest: ManifestProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    use relix_core::capability::CapabilityDescriptor;

    if cfg.controller.node_type == "memory" {
        let raw = cfg.memory.clone().ok_or_else(|| {
            "node_type=memory requires a [memory] section with db_path".to_string()
        })?;
        let mem_cfg: crate::nodes::memory::MemoryConfig = raw
            .try_into()
            .map_err(|e: toml::de::Error| format!("[memory] parse: {e}"))?;
        let store = std::sync::Arc::new(crate::nodes::memory::MemoryStore::open(&mem_cfg)?);
        crate::nodes::memory::register(bridge, store);
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
                "memory.search",
                "FTS5 substring search across all stored turns.",
                &["search", "memory"],
                &["reads:internal"],
            ),
        ];
        for (m, desc, cats, tags) in memory_caps {
            manifest.add_capability(
                CapabilityDescriptor::unary(*m)
                    .with_description(*desc)
                    .with_categories(cats.iter().map(|s| (*s).into()))
                    .with_sensitivity(tags.iter().map(|s| (*s).into())),
            );
        }
        tracing::info!(
            db = %mem_cfg.db_path.display(),
            "memory node: registered memory.write_turn / memory.recent_for_session / memory.search"
        );
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
        crate::nodes::ai::register(bridge, provider.clone(), default_model.clone());
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
                .with_environment_requirements([format!("provider:{provider_name}")]),
        );
        tracing::info!(
            provider = %provider_name,
            default_model = %default_model,
            "ai node: registered ai.chat"
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
        }
        crate::nodes::coordinator::register(bridge, store);
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
        ];
        for (m, desc, cats) in coord_caps {
            manifest.add_capability(
                CapabilityDescriptor::unary(*m)
                    .with_description(*desc)
                    .with_categories(cats.iter().map(|s| (*s).into())),
            );
        }
        tracing::info!(
            db = %coord_cfg.db_path.display(),
            max_list = coord_cfg.max_list,
            recovery_scan = coord_cfg.recovery_scan,
            "coordinator node: registered task.create / update / event / get / list / count / list_cursor / events / recover / attempts / retry / export / compact_events / edges / note / mark_investigation / pause / resume / lineage / recent_events / interruption_check / observe_interruption / freeze / unfreeze / record_spawned / record_delegated / record_awaited / transition_check / subtree_metrics"
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
        let backend = std::sync::Arc::new(crate::nodes::tool::ToolBackend::new(tool_cfg.clone())?);
        crate::nodes::tool::register(bridge, backend);
        let desc = crate::nodes::tool::capability_descriptor();
        manifest.add_capability(desc.clone());
        // B1: tool.web_extract — pure HTML parser. Lives on the same
        // tool node so it shares identity / admission / audit / pool
        // setup. Distinct from tool.web_fetch (no network surface).
        manifest.add_capability(crate::nodes::tool::web_extract::capability_descriptor());
        // CW3: tool.web_get + tool.web_search — composed over the
        // same web_fetch pipeline. Both always advertise alongside
        // web_fetch since they share the SSRF/pin/redirect machinery.
        manifest.add_capability(crate::nodes::tool::web_tools::web_get_descriptor());
        manifest.add_capability(crate::nodes::tool::web_tools::web_search_descriptor());
        // PH-WEB-ROBOTS: tool.web.robots_check — robots.txt sniff.
        // Same SSRF + pin + redirect machinery as web_fetch.
        manifest.add_capability(crate::nodes::tool::web_robots::robots_check_descriptor());
        // B2: jailed filesystem subsystem. Only advertised when
        // `[tool.fs]` is configured -- node-type tool with no
        // `[tool.fs]` keeps fs out of the manifest.
        if tool_cfg.fs.is_some() {
            manifest.add_capability(crate::nodes::tool::fs::descriptor_read());
            manifest.add_capability(crate::nodes::tool::fs::descriptor_write());
            manifest.add_capability(crate::nodes::tool::fs::descriptor_search());
            manifest.add_capability(crate::nodes::tool::fs::descriptor_patch());
            // CW2: tool.list_dir — read-side directory
            // enumeration with stable pagination. Same jail.
            manifest.add_capability(crate::nodes::tool::fs::descriptor_list());
            // PH-FS-PARITY1: tool.append_file + tool.patch_preview.
            // Same jail; append is strictly additive (refuses to
            // create), patch_preview is read-only dry-run.
            manifest.add_capability(crate::nodes::tool::fs::descriptor_append());
            manifest.add_capability(crate::nodes::tool::fs::descriptor_patch_preview());
            // PH-FS-PARITY2: tool.binary_sniff — classify a file
            // as text/binary by reading the first 8 KiB. Same jail.
            manifest.add_capability(crate::nodes::tool::fs::descriptor_binary_sniff());
            // PH-FS-PARITY4: tool.fs.audit_recent — operator
            // snapshot of the most recent successful mutations
            // (write / append / patch) on the jail. Bounded
            // in-memory ring.
            manifest.add_capability(crate::nodes::tool::fs::descriptor_audit_recent());
        }
        // B3: tool.pdf — only advertised when [tool.pdf] is configured.
        if tool_cfg.pdf.is_some() {
            manifest.add_capability(crate::nodes::tool::pdf::capability_descriptor());
        }
        // CW1: tool.terminal.run — sandboxed shell. Only advertised
        // when [tool.terminal] is configured AND construction
        // succeeds (allowlist validation may fail; the
        // descriptor advertisement matches the actual
        // registration so consumers don't see a phantom
        // capability).
        if let Some(term_cfg) = tool_cfg.terminal.as_ref()
            && crate::nodes::tool::terminal::TerminalBackend::new(term_cfg.clone()).is_ok()
        {
            manifest.add_capability(crate::nodes::tool::terminal_descriptor());
            // PH-TERM-SESSIONS: tool.terminal.sessions — live
            // run registry snapshot. Always co-advertised when
            // the terminal config validates.
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_sessions());
            // PH-TERM-AUDIT: tool.terminal.audit_recent — bounded
            // ring of completed runs (success + timed-out + cancelled).
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_audit_recent());
            // PH-TERM-CANCEL: tool.terminal.cancel — cooperative
            // termination of a live run by session id.
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_cancel());
        }
        // CW4: tool.browser.* — only advertised when
        // [tool.browser] is configured. Honest: the descriptors
        // ship even with backend="none" so operators see the
        // surface; the runtime returns BackendNotConnected
        // until a real backend lands.
        if let Some(br_cfg) = tool_cfg.browser.as_ref()
            && crate::nodes::tool::browser::build_backend(br_cfg).is_ok()
        {
            manifest.add_capability(crate::nodes::tool::browser::descriptor_open_session());
            manifest.add_capability(crate::nodes::tool::browser::descriptor_close_session());
            manifest.add_capability(crate::nodes::tool::browser::descriptor_navigate());
            manifest.add_capability(crate::nodes::tool::browser::descriptor_get_text());
            manifest.add_capability(crate::nodes::tool::browser::descriptor_screenshot());
            manifest.add_capability(crate::nodes::tool::browser::descriptor_list_sessions());
        }
        // CW5: tool.mcp.* — registry + discovery surface
        // advertised when [tool.mcp] is configured AND the
        // registry validates (duplicate ids, bad transport,
        // etc. fail-closed). Invoke still returns
        // RuntimeNotConnected until the live client lands.
        if let Some(mcp_cfg) = tool_cfg.mcp.as_ref()
            && crate::nodes::tool::mcp::validate_config(mcp_cfg).is_ok()
        {
            manifest.add_capability(crate::nodes::tool::mcp::descriptor_list_servers());
            manifest.add_capability(crate::nodes::tool::mcp::descriptor_list_tools());
            manifest.add_capability(crate::nodes::tool::mcp::descriptor_invoke());
        }
        tracing::info!(
            max_bytes = tool_cfg.max_bytes,
            timeout_secs = tool_cfg.timeout_secs,
            max_redirects = tool_cfg.max_redirects,
            allow_http = tool_cfg.allow_http,
            method = %desc.method_name,
            sensitivity = ?desc.sensitivity_tags,
            cw3 = "tool.web_get, tool.web_search",
            "tool node: registered tool.web_fetch + CW3 web_tools"
        );
    }
    // web_bridge / demo node types are no-ops today; their handlers ship in
    // later milestones. node.health is always available via builtins.
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
