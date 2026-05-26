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
    let mut startup_wiring: Option<StartupWiring> = None;
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
        register_node_type_handlers(&mut bridge, &cfg, manifest.clone(), &mut startup_wiring)?;
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

    // Post-startup wiring (AI memory injection). Spawns a small
    // discovery task that builds a MeshClient pointing at the
    // memory peer and populates the `OnceCell` the ai.chat
    // handler already captured. Failure is non-fatal — the AI
    // node still serves chat, just without memory injection.
    match startup_wiring.take() {
        Some(StartupWiring::AiMemory {
            cell,
            cfg: Some(memcfg),
        }) => {
            let key_path = cfg.identity.key_path.clone();
            tokio::spawn(async move {
                populate_ai_memory_cell(cell, memcfg, key_path).await;
            });
        }
        Some(StartupWiring::MemoryCurator {
            ai_cell,
            coord_cell,
            state,
            cfg: ccfg,
            embedding_cell,
            embedding_cfg,
        }) => {
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
                tracing::info!("memory curator: no [memory.curator.ai_peer]; AI dispatcher unset");
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
        Some(StartupWiring::MemoryEmbedding { cell, cfg: epeer }) => {
            let key_path = cfg.identity.key_path.clone();
            tokio::spawn(async move {
                populate_memory_embedding_cell(cell, epeer, key_path).await;
            });
        }
        Some(StartupWiring::Telegram { cell, cfg: tg_cfg }) => {
            let key_path = cfg.identity.key_path.clone();
            tokio::spawn(async move {
                populate_telegram_outbound_cell(cell, tg_cfg, key_path).await;
            });
        }
        Some(StartupWiring::Discord { cell, cfg: dc_cfg }) => {
            let key_path = cfg.identity.key_path.clone();
            tokio::spawn(async move {
                populate_discord_outbound_cell(cell, dc_cfg, key_path).await;
            });
        }
        Some(StartupWiring::Slack { cell, cfg: sl_cfg }) => {
            let key_path = cfg.identity.key_path.clone();
            tokio::spawn(async move {
                populate_slack_outbound_cell(cell, sl_cfg, key_path).await;
            });
        }
        _ => {}
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
        client_key: client_key_bytes,
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
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

async fn run_approval_expire_loop(
    agent_store: Arc<crate::nodes::coordinator::agent::AgentStore>,
    task_store: Arc<crate::nodes::coordinator::TaskStore>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
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
fn register_agent_capabilities(
    bridge: &mut crate::dispatch::DispatchBridge,
    agent_store: Arc<crate::nodes::coordinator::agent::AgentStore>,
    task_store: Arc<crate::nodes::coordinator::TaskStore>,
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
        bridge.register(
            "coord.approval.decide",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let resume = resume.clone();
                let fail = fail.clone();
                async move { handlers::handle_approval_decide(&s, &ctx, &resume, &fail) }
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
    bridge.set_agent_gate(crate::dispatch::AgentGateBindings {
        store: bindings_store,
        describe: Arc::new(|_method: &str| None),
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
        client_key: client_key_bytes,
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
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
        client_key: client_key_bytes,
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
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
        client_key: client_key_bytes,
        peers: peers_file,
        // Use the AI deadline as the outer bound — it's the
        // longest of the three configured per-call deadlines
        // for typical configs.
        deadline_secs: cfg.ai_peer.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
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
        client_key: client_key_bytes,
        peers: peers_file,
        deadline_secs: cfg.ai_peer.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
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
        client_key: client_key_bytes,
        peers: peers_file,
        deadline_secs: cfg.ai_peer.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
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
        client_key: client_key_bytes,
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
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
        client_key: client_key_bytes,
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
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
        client_key: client_key_bytes,
        peers: peers_file,
        deadline_secs: cfg.deadline_secs,
        overall_timeout: std::time::Duration::from_secs(10),
        local_port: None,
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
                    match crate::plugin::PluginLoader::spawn(manifest, path, 10, 30).await {
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
}

// Parse `[guardrails.input]` from the top-level [guardrails]
// TOML section into an InputGuardrail instance. Absent /
// disabled / unparseable produces a permissive guardrail so
// the AI controller behaves exactly as before.
fn build_input_guardrail(cfg: &ControllerConfig) -> crate::nodes::ai::guardrails::InputGuardrail {
    use crate::nodes::ai::guardrails::{InputGuardrail, input::InputGuardrailConfig};
    let Some(raw) = cfg.guardrails.clone() else {
        return InputGuardrail::permissive();
    };
    #[derive(serde::Deserialize, Default)]
    struct GuardrailsBlock {
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
    match parsed.input {
        Some(ic) => InputGuardrail::from_config(&ic),
        None => InputGuardrail::permissive(),
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
    Ok(Some(crate::nodes::memory::LayeredContext {
        store,
        qdrant,
        score_threshold,
    }))
}

/// Register node-type-specific capabilities based on `[controller] node_type`.
///
/// - `memory` → SQLite + FTS5 memory store (M7).
/// - Other types (`ai`, `tool`, `web_bridge`, `demo`, ...) are no-ops until
///   their handlers ship in later milestones; the controller still serves
///   the default `node.health` capability so it can participate in chained
///   orchestration today.
fn register_node_type_handlers(
    bridge: &mut DispatchBridge,
    cfg: &ControllerConfig,
    manifest: ManifestProvider,
    out: &mut Option<StartupWiring>,
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
        crate::nodes::memory::register(
            bridge,
            store.clone(),
            curator_ai_cell.clone(),
            embedding_cell.clone(),
            embedding_model,
            curator_handler_cfg,
            layered_ctx.clone(),
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
            *out = Some(StartupWiring::MemoryCurator {
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
                *out = Some(StartupWiring::MemoryEmbedding {
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
                let pipeline = crate::nodes::memory::embedder::EmbeddingPipeline::new(
                    layered.store.clone(),
                    layered.qdrant.clone(),
                    embed_fn,
                    emb_cfg.batch_size,
                    emb_cfg.interval_secs,
                );
                pipeline.spawn();
                tracing::info!(
                    batch_size = emb_cfg.batch_size,
                    interval_secs = emb_cfg.interval_secs,
                    "memory node: embedding pipeline spawned"
                );
            }
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
        crate::nodes::ai::register(
            bridge,
            provider.clone(),
            default_model.clone(),
            memory_cell.clone(),
            soul_cache,
            skill_matcher,
            input_guardrail,
        );
        // Hand back to run() so the post-rpc::Client setup can
        // build a MemoryDispatcher into the cell when
        // ai_cfg.memory_peer is configured.
        *out = Some(StartupWiring::AiMemory {
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
        crate::nodes::coordinator::register(bridge, store.clone(), auto_skill_cfg);
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
        let agent_store = std::sync::Arc::new(
            crate::nodes::coordinator::agent::AgentStore::open(&coord_cfg.db_path)
                .map_err(|e| format!("[coordinator] agent store open: {e}"))?,
        );
        register_agent_capabilities(bridge, agent_store.clone(), store.clone());
        let agent_caps: &[(&str, &str, &[&str])] = &[
            (
                "agent.create",
                "Create an agent profile. Arg: name|role|title|department|team|created_by|subject_id|risk_ceiling.",
                &["agent", "persist"],
            ),
            ("agent.get", "Read one agent profile.", &["agent", "read"]),
            (
                "agent.list",
                "List agent profiles (optionally filtered by subject_id).",
                &["agent", "read"],
            ),
            (
                "agent.update",
                "Update one of {status, role, title, department, team, surface_allowlist, risk_ceiling, allow_categories, deny_categories, allow_sensitivity_tags, deny_sensitivity_tags, approval_required_categories, approval_timeout_secs}.",
                &["agent", "mutate"],
            ),
            (
                "agent.delete",
                "Soft delete: flip the profile's status to `disabled`.",
                &["agent", "mutate"],
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
            tokio::spawn(async move {
                run_approval_expire_loop(agent_store_for_expire, task_store_for_expire).await;
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
        crate::nodes::telegram::register(bridge, state.clone(), ring.clone());
        // Spawn the long-poll loop now. The loop checks the
        // out_cell on every tick and gracefully degrades when
        // the mesh client isn't wired yet (sends a fallback
        // reply rather than crashing).
        let api = relix_telegram::LiveBotApi::new(token);
        let state_for_loop = state.clone();
        let ring_for_loop = ring.clone();
        let cfg_for_loop = Arc::new(tg_cfg.clone());
        let out_for_loop = out_cell.clone();
        tokio::spawn(async move {
            crate::nodes::telegram::run_telegram_controller_with_api(
                api,
                out_for_loop,
                state_for_loop,
                ring_for_loop,
                notifier,
                cfg_for_loop,
            )
            .await;
        });
        // Hand back to run() so the post-rpc::Client setup
        // can dial memory + ai + coord and publish the
        // outbound client into the cell.
        let tg_cfg_for_wiring = tg_cfg.clone();
        *out = Some(StartupWiring::Telegram {
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
        crate::nodes::discord::register(
            bridge,
            state.clone(),
            ring.clone(),
            dc_cfg.channel_id.clone(),
        );
        let api = relix_discord::LiveDiscordApi::new(token);
        let state_for_loop = state.clone();
        let ring_for_loop = ring.clone();
        let cfg_for_loop = Arc::new(dc_cfg.clone());
        let out_for_loop = out_cell.clone();
        tokio::spawn(async move {
            crate::nodes::discord::run_discord_controller_with_api(
                api,
                out_for_loop,
                state_for_loop,
                ring_for_loop,
                cfg_for_loop,
            )
            .await;
        });
        let dc_cfg_for_wiring = dc_cfg.clone();
        *out = Some(StartupWiring::Discord {
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
        crate::nodes::slack::register(
            bridge,
            state.clone(),
            ring.clone(),
            sl_cfg.channel_id.clone(),
        );
        let api = relix_slack::LiveSlackApi::new(token);
        let state_for_loop = state.clone();
        let ring_for_loop = ring.clone();
        let cfg_for_loop = Arc::new(sl_cfg.clone());
        let out_for_loop = out_cell.clone();
        tokio::spawn(async move {
            crate::nodes::slack::run_slack_controller_with_api(
                api,
                out_for_loop,
                state_for_loop,
                ring_for_loop,
                cfg_for_loop,
            )
            .await;
        });
        let sl_cfg_for_wiring = sl_cfg.clone();
        *out = Some(StartupWiring::Slack {
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
            let loaded = match tokio::task::block_in_place(|| {
                host_handle.block_on(crate::plugin::PluginLoader::spawn(
                    manifest_for_spawn,
                    manifest_path_for_spawn,
                    10,
                    30,
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
        // PH-WEB-POST: tool.web.post — POST with body + cookie
        // headers, same SSRF + DNS pin posture as web_fetch.
        manifest.add_capability(crate::nodes::tool::web_tools::web_post_descriptor());
        // PH-DASH-BLOCKLIST: tool.web.blocklist_summary — read
        // the operator-curated `[tool] blocked_hosts` set. Pure
        // config read, no I/O.
        manifest.add_capability(crate::nodes::tool::web_tools::web_blocklist_summary_descriptor());
        // PH-WEB-ROBOTS: tool.web.robots_check — robots.txt sniff.
        // Same SSRF + pin + redirect machinery as web_fetch.
        manifest.add_capability(crate::nodes::tool::web_robots::robots_check_descriptor());
        // PH-PDF-CHUNK: tool.text.chunk — pure CPU text chunker
        // for retrieval / context-window-fit use cases. Always
        // advertised when the tool node is up.
        manifest.add_capability(crate::nodes::tool::text_chunk::capability_descriptor());
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
            // PH-FS-FUZZY: tool.fuzzy_replace — Hermes-style
            // whitespace-tolerant text edit. Same jail.
            manifest.add_capability(crate::nodes::tool::fs::descriptor_fuzzy_replace());
            // PH-FS-TREE: tool.fs.tree — depth-capped recursive
            // directory walk.
            manifest.add_capability(crate::nodes::tool::fs::descriptor_tree());
            // PH-FS-STAT: tool.fs.stat — single-path metadata.
            manifest.add_capability(crate::nodes::tool::fs::descriptor_stat());
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
            // PH-TERM-STREAM1: tool.terminal.tail — polling-cursor
            // stream tail of a live run's stdout / stderr buffer.
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_tail());
            // PH-TERM-SPAWN: tool.terminal.spawn — fire-and-forget
            // background variant of tool.terminal.run.
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_spawn());
            // PH-TERM-SHELL: tool.terminal.shell.{open,input,close}
            // — persistent shell sessions. open/input/close are
            // always advertised when terminal is configured; the
            // open handler refuses fail-closed when `allowed_shells`
            // is empty, so the surface is honest about whether
            // shells are actually usable.
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_shell_open());
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_shell_input());
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_shell_close());
            // PH-TERM-CONTROL: tool.terminal.shell.control —
            // convenience writer for named control chars
            // (etx/eot/tab/enter/esc/backspace/...).
            manifest.add_capability(crate::nodes::tool::terminal::descriptor_shell_control());
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
            // W2-002a: click / type_text / wait_for_selector.
            manifest.add_capability(crate::nodes::tool::browser::descriptor_click());
            manifest.add_capability(crate::nodes::tool::browser::descriptor_type_text());
            manifest.add_capability(crate::nodes::tool::browser::descriptor_wait_for_selector());
            // W2-002f: capture_read serves failure screenshots
            // back to the dashboard. Advertised even when
            // `screenshot_on_failure_dir` is None — the
            // handler returns INVALID_ARGS with a clear
            // "not configured" message so operators see why.
            manifest.add_capability(crate::nodes::tool::browser::descriptor_capture_read());
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
