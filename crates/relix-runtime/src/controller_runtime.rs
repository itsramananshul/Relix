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
    register_node_type_handlers(&mut bridge, &cfg, manifest.clone())?;

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
        }
        tracing::info!(
            max_bytes = tool_cfg.max_bytes,
            timeout_secs = tool_cfg.timeout_secs,
            max_redirects = tool_cfg.max_redirects,
            allow_http = tool_cfg.allow_http,
            method = %desc.method_name,
            sensitivity = ?desc.sensitivity_tags,
            "tool node: registered tool.web_fetch"
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
