//! relix-web-bridge — local HTTP bridge that turns `POST /chat` (and the
//! OpenAI-compatible `POST /v1/chat/completions`) into a Relix SOL chat
//! orchestration on the mesh.
//!
//! ## Endpoints
//!
//! | Method | Path                     | Body / Stream                                    |
//! |--------|--------------------------|--------------------------------------------------|
//! | GET    | `/health`                | `200 ok\n`                                       |
//! | POST   | `/chat`                  | JSON in / JSON out                               |
//! | POST   | `/chat/stream`           | JSON in / `text/event-stream` out (chunk + done) |
//! | GET    | `/v1/models`             | OpenAI models list                               |
//! | POST   | `/v1/chat/completions`   | OpenAI request → JSON or OpenAI SSE              |
//!
//! See `docs/streaming-and-openai-shim.md` for the integration story and the
//! alpha simplifications backing the OpenAI shim (SIMP-019, SIMP-020).
//!
//! ## What the bridge is NOT
//!
//! - Not a central gateway. It calls the mesh as a normal peer identity;
//!   responders run the full admission pipeline (identity + policy + audit)
//!   on every call.
//! - Not the owner of any AI provider key. Provider keys live only on the
//!   AI node (see `docs/provider-configuration.md`).
//! - Not an orchestrator. The SOL flow file is the orchestration; the
//!   bridge only renders + runs it.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::{
    Router,
    extract::Request,
    middleware::Next,
    response::Response,
    routing::{get, patch, post, put},
};
use clap::Parser;

/// H15: per-request latency-tracking middleware. Wraps every
/// route registered on the bridge router and emits one
/// `bridge: route` info line per request with method, path,
/// status, and wall-clock elapsed_ms. No in-process state —
/// operators wire this into their tracing collector (Grafana,
/// Loki, etc.) to derive per-route p50/p95 over arbitrary
/// windows. Streaming responses log the time to first response
/// header, NOT the total stream duration — the inner handler
/// closes its span when it returns the `Response`, which for
/// SSE is when the response head is flushed.
async fn route_latency_log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    // INFO for 2xx/3xx, WARN for 4xx, ERROR for 5xx — gives
    // operators a quick log-level filter on outliers.
    if status.is_success() || status.is_redirection() {
        tracing::info!(
            http.method = %method,
            http.path = %path,
            http.status = status.as_u16(),
            http.elapsed_ms = elapsed_ms,
            "bridge: route"
        );
    } else if status.is_client_error() {
        tracing::warn!(
            http.method = %method,
            http.path = %path,
            http.status = status.as_u16(),
            http.elapsed_ms = elapsed_ms,
            "bridge: route (client error)"
        );
    } else {
        tracing::error!(
            http.method = %method,
            http.path = %path,
            http.status = status.as_u16(),
            http.elapsed_ms = elapsed_ms,
            "bridge: route (server error)"
        );
    }
    resp
}

mod activity;
mod adapters;
mod agent;
mod agent_memory;
mod agent_metrics;
#[cfg(test)]
mod agent_metrics_mini_mesh_test;
#[cfg(test)]
mod agent_token_mini_mesh_test;
mod agents_access;
#[cfg(test)]
mod alert_dispatch_mini_mesh_test;
mod approval;
#[cfg(test)]
mod approval_get_mini_mesh_test;
mod audit_tenants;
mod auth;
mod belief;
mod blocklist;
mod bridge_back;
#[cfg(test)]
mod brief_doc_lock_mini_mesh_test;
#[cfg(test)]
mod brief_interaction_mini_mesh_test;
mod browser_captures;
mod browser_sessions;
mod budget;
mod capabilities;
mod channels;
mod chat;
mod companion;
mod confidence;
#[cfg(test)]
mod confidence_mini_mesh_test;
mod config;
mod config_api;
mod control_plane;
mod credentials;
mod cron;
mod dashboard;
mod dashboard_auth;
mod delegate;
mod discord;
mod dispatch_stats;
mod email;
#[cfg(test)]
mod email_mini_mesh_test;
mod execution;
mod export;
mod flow;
mod fs_audit;
mod guardrails;
mod identity_session;
mod intervention_audit;
mod judge;
mod knowledge;
#[cfg(test)]
mod knowledge_mini_mesh_test;
#[cfg(test)]
mod legacy_token_full_stack_integration_test;
mod lifecycle;
mod logs;
mod mcp;
mod mcp_audit;
mod memory_curator;
mod memory_embed;
mod memory_gap5;
mod memory_inspect;
mod memory_pii;
#[cfg(test)]
mod memory_pii_mini_mesh_test;
mod messaging;
mod metrics;
mod observability;
#[cfg(test)]
mod observability_mini_mesh_test;
mod openai;
mod os_secure;
mod peer_call;
mod pii;
mod planning;
#[cfg(test)]
mod planning_mini_mesh_test;
mod plugins;
mod policy_denials;
mod policy_simulate;
mod policy_tenants;
#[cfg(test)]
mod prime_status_mini_mesh_test;
mod provenance;
mod rate_limit;
mod reasoning;
mod routing;
#[cfg(test)]
mod run_apply_mini_mesh_test;
#[cfg(test)]
mod runs_stream_mini_mesh_test;
mod schema;
mod secrets;
mod secrets_available;
mod security_headers;
mod session_search;
mod sessions_obs;
mod skills;
mod slack;
mod sol_validate;
mod spine;
mod sse;
#[cfg(test)]
mod streaming_mini_mesh_test;
mod task_recorder;
mod tasks;
mod telegram;
mod tenant;
#[cfg(test)]
mod tenant_isolation_full_stack_test;
mod term_audit;
mod tool_screen;
mod tools;
#[cfg(test)]
mod tools_mini_mesh_test;
mod topology;
mod training;
#[cfg(test)]
mod training_mini_mesh_test;
#[cfg(test)]
mod usage_mini_mesh_test;
mod validate;
mod workflows;
#[cfg(test)]
mod workflows_mini_mesh_test;
mod workspaces;
mod ws;
mod yaml_validate;

use crate::config::{AppState, BridgeConfig};

#[derive(Parser, Debug)]
#[command(
    name = "relix-web-bridge",
    version,
    about = "Local HTTP bridge that triggers a Relix SOL chat flow."
)]
struct Args {
    /// Path to the bridge config TOML (see `configs/web-bridge.toml`).
    /// Required to RUN the bridge; optional for the `reset-admin` subcommand.
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Local maintenance subcommands. With NO subcommand the binary runs the
/// bridge server — the default and ONLY network-facing mode.
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Reset the local dashboard admin password — LOCAL operator recovery
    /// only (there is NO network/unauthenticated path to this). Writes a
    /// new Argon2id credential to the admin file; restart the bridge for it
    /// to take effect and to drop existing sessions.
    ResetAdmin(ResetAdminArgs),
}

#[derive(clap::Args, Debug)]
struct ResetAdminArgs {
    /// Operate directly on this `dashboard-admin.json` (overrides --config).
    #[arg(long)]
    admin_file: Option<PathBuf>,
    /// New admin username. Defaults to the existing username, else "admin".
    #[arg(long)]
    username: Option<String>,
    /// New admin password (min 8 chars). If omitted, a strong random one is
    /// generated and printed once.
    #[arg(long)]
    password: Option<String>,
}

/// Bridge-layer errors. Used at startup (config / identity bundle load).
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("config: {0}")]
    Config(String),
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A strong, copy-friendly random password for the local admin reset:
/// `relix-` + 24 hex chars (96 bits of OS entropy).
fn generate_password() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    format!("relix-{}", hex::encode(buf))
}

/// `relix-web-bridge reset-admin` — LOCAL operator recovery for a forgotten
/// dashboard admin password. Resolves the admin file (explicit `--admin-file`
/// → `--config`-derived → `~/.relix/dashboard-admin.json`), writes a new
/// Argon2id credential, and prints the new username (+ generated password
/// once). Never prints/reads the old secret; never opens a network surface.
fn run_admin_reset(
    config: Option<&std::path::Path>,
    ra: &ResetAdminArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let admin_path: PathBuf = if let Some(p) = &ra.admin_file {
        p.clone()
    } else if let Some(cfg_path) = config {
        let text = std::fs::read_to_string(cfg_path)
            .map_err(|e| format!("read config {}: {e}", cfg_path.display()))?;
        let cfg: BridgeConfig = toml::from_str(&text).map_err(|e| format!("parse config: {e}"))?;
        let token = crate::config::resolve_bridge_token_path(&cfg);
        crate::dashboard_auth::admin_path_for_token(&token)
    } else {
        crate::config::default_admin_path()
    };

    // Keep the existing username unless one was given; fall back to "admin".
    let existing = crate::dashboard_auth::read_admin_username(&admin_path);
    let username = ra
        .username
        .clone()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .or_else(|| existing.clone())
        .unwrap_or_else(|| "admin".to_string());

    let (password, generated) = match &ra.password {
        Some(p) => (p.clone(), false),
        None => (generate_password(), true),
    };

    crate::dashboard_auth::reset_admin_credential(&admin_path, &username, &password)
        .map_err(|e| format!("reset failed: {e}"))?;

    let was_new = existing.is_none();
    println!();
    println!(
        "Relix dashboard admin {} (LOCAL operator recovery).",
        if was_new { "created" } else { "reset" }
    );
    println!("  admin file : {}", admin_path.display());
    println!("  username   : {username}");
    if generated {
        println!("  password   : {password}");
        println!(
            "  (generated — copy it now; only the Argon2id hash is stored, so it is not shown again)"
        );
    } else {
        println!("  password   : (the value you passed via --password)");
    }
    println!();
    println!(
        "Restart the bridge for the new credential to take effect and to drop existing sessions."
    );
    println!(
        "There is NO remote/unauthenticated reset — this command only runs locally on this machine."
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Dashboard Section 18: the LogRing must be installed as a
    // tracing layer BEFORE any event fires so the ring captures
    // the bridge's own startup output. We construct it here,
    // hand it to the tracing registry, and then thread the same
    // handle into AppState.log_ring so the SSE endpoint and the
    // layer share one buffer.
    let log_ring = crate::logs::LogRing::new();
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .with(crate::logs::LogRingLayer::new(log_ring.clone()))
            .init();
    }

    let args = Args::parse();
    // Local maintenance subcommands run + exit WITHOUT booting the mesh /
    // server. `reset-admin` is a filesystem operation an operator runs on
    // their own machine — never a network surface.
    if let Some(Command::ResetAdmin(ra)) = &args.command {
        return run_admin_reset(args.config.as_deref(), ra);
    }
    let config_path = args
        .config
        .clone()
        .ok_or_else(|| "--config <bridge.toml> is required to run the bridge".to_string())?;
    let cfg: BridgeConfig = {
        let text = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("read config {}: {e}", config_path.display()))?;
        toml::from_str(&text).map_err(|e| format!("parse config: {e}"))?
    };
    let mut state = AppState::try_new(cfg.clone())?;
    // Replace the default LogRing baked into AppState by the
    // try_new ctor with the one we installed on the tracing
    // registry above — both halves must point at the same
    // buffer.
    state.log_ring = log_ring;

    // P3: surface the log-stream redaction posture at boot.
    // The default is `redact_stream = true`; operators who flip
    // it off see a loud WARN so the posture is obvious.
    if state.cfg.logging.redact_stream {
        tracing::info!(
            "log stream redaction ENABLED (P3) — secrets (bearer tokens, \
             API keys, JWTs, AWS credentials) are masked before streaming"
        );
    } else {
        tracing::warn!(
            "log stream redaction is DISABLED (logging.redact_stream = false) — \
             raw log content is sent over /v1/logs/stream. Re-enable for \
             production deployments."
        );
    }

    // W7: spawn the OTel flush loop on startup if the bridge's
    // `[observability.otel]` block enabled the exporter. The
    // exporter is shared with the ObservabilityContext via Arc;
    // record_event buffers; the spawned loop ships every 5s.
    if let Some(exporter) = state.otel_exporter.clone() {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                let _ = exporter.flush().await;
            }
        });
        tracing::info!("bridge observability: spawned OTLP flush loop (interval=5s)");
    }

    // M10 + M11: discovery pass that *also* hands back a long-lived
    // MeshClient. The libp2p transport + dial cost is now paid once at
    // startup; every /chat thereafter reuses it.
    let discovery_opts = relix_runtime::manifest::DiscoveryOptions {
        identity_bundle: state.identity_bundle.clone(),
        client_key: state.client_key.clone(),
        peers: state.peers.clone(),
        deadline_secs: state.cfg.transport.deadline_secs.min(10),
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
        source_key_registry: None,
    };
    match relix_runtime::manifest::discover_and_pin(discovery_opts).await {
        Some((discovered, mesh_client)) => {
            let entries = discovered.entries();
            tracing::info!(
                peers = entries.len(),
                methods = ?discovered.all_methods(),
                pooled_peer_ids = mesh_client.peer_ids().len(),
                "bridge discovery complete (transport pooled for M11)"
            );
            let cache_arc = std::sync::Arc::new(discovered);
            state.manifest_cache = cache_arc.clone();

            // RELA-24: build the discoverable tool registry from the
            // tool node's advertised capabilities now that discovery
            // has pulled its manifest. Backs `/v1/tools`,
            // `/v1/tools/search`, and `/v1/tools/manifest`. Stays the
            // empty fallback when no tool peer was discovered.
            let tool_registry = crate::tools::registry_from_manifest(&cache_arc);
            tracing::info!(
                tools = tool_registry.len(),
                "bridge: tool registry built from discovered capabilities"
            );
            state.tool_registry = tool_registry;

            // A.4: spawn a background manifest-refresh loop. Every 60s
            // re-pulls each peer's manifest so capabilities added /
            // removed after bridge startup become visible without a
            // restart, and so dropped connections are re-dialled
            // proactively (in addition to the per-call reconnect retry
            // inside `MeshClient::call`).
            let refresh_handle = mesh_client
                .clone()
                .spawn_refresh_loop(cache_arc, std::time::Duration::from_secs(60));
            tracing::info!(
                period_secs = 60,
                "mesh: background manifest refresh task spawned"
            );
            // Detach: the loop runs for the lifetime of the bridge
            // process; we never `.await` the handle. `drop` silences
            // clippy::let_underscore_future.
            drop(refresh_handle);

            let mesh_arc = std::sync::Arc::new(mesh_client);

            // B1.1 / B1.9: optional coordinator integration. We only
            // build the TaskRecorder when both (a) the config names a
            // coordinator alias AND (b) the alias resolves in the
            // address book — otherwise everything stays None and the
            // bridge runs without persistence (fail-soft).
            if let Some(coord_cfg) = state.cfg.coordinator.as_ref() {
                if mesh_arc.peer_id_for(&coord_cfg.alias).is_some() {
                    let recorder = task_recorder::TaskRecorder::new(
                        mesh_arc.clone(),
                        coord_cfg.alias.clone(),
                        state.identity_bundle.clone(),
                        state.cfg.transport.deadline_secs,
                    );
                    state.task_recorder = Some(recorder);
                    tracing::info!(
                        coordinator_alias = %coord_cfg.alias,
                        "bridge: task persistence enabled (coordinator reachable at startup)"
                    );
                } else {
                    if coord_cfg.required {
                        return Err(format!(
                            "bridge: [coordinator] required=true but alias '{}' was not discovered; refusing to start without durable task persistence",
                            coord_cfg.alias
                        )
                        .into());
                    }
                    tracing::warn!(
                        coordinator_alias = %coord_cfg.alias,
                        "bridge: [coordinator] alias configured but peer not in discovered set; task persistence disabled (chat still works)"
                    );
                }
            } else {
                tracing::info!(
                    "bridge: no [coordinator] section in config; task persistence disabled (chat still works)"
                );
            }

            state.mesh_client = Some(mesh_arc);
        }
        None => {
            if state
                .cfg
                .coordinator
                .as_ref()
                .is_some_and(|coord| coord.required)
            {
                return Err(
                    "bridge: [coordinator] required=true but mesh discovery did not return a client; refusing to start without durable task persistence"
                        .into(),
                );
            }
            tracing::warn!(
                "bridge discovery did not return a mesh client; chat requests will fall back to per-request transport"
            );
        }
    }
    // Background lifecycle diff task: every 5s, snapshot the
    // manifest cache + diff against the previous snapshot to
    // record join / freshness / drop transitions. Provides the
    // server-side history operators see at /v1/topology/events.
    {
        let cache = state.manifest_cache.clone();
        let log = state.lifecycle_log.clone();
        tokio::spawn(async move {
            // Seed snapshot immediately (no events emitted) so
            // the next tick can detect real transitions.
            log.diff_and_record(&cache, unix_secs());
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                log.diff_and_record(&cache, unix_secs());
            }
        });
        tracing::info!(period_secs = 5, "bridge: lifecycle diff task spawned");
    }

    let addr: SocketAddr = state
        .cfg
        .bridge
        .listen_addr
        .parse()
        .map_err(|e| format!("listen_addr: {e}"))?;

    // Capture the token + its on-disk path now, before `state` is
    // moved into the router below. The printed banner after
    // `serve` would otherwise reach for a moved value.
    let bridge_token_value = state.bridge_token.value().to_string();
    let bridge_token_path = state.bridge_token.path().to_path_buf();

    // M78: production-posture guard. The bridge has no HTTP
    // auth in alpha (see bridge-invariants.md). When the
    // operator binds it to anything other than loopback we
    // emit a single loud WARN at startup so the
    // "I forgot to put a reverse proxy in front" mistake is
    // caught immediately, before traffic flows. Honest about
    // scope: this does NOT refuse to start — operators are
    // sometimes deliberately running behind a mTLS tunnel or
    // similar; the warn surfaces the gap, not the verdict.
    if !addr.ip().is_loopback() {
        tracing::warn!(
            listen_addr = %addr,
            "bridge: listen_addr is not loopback — the bridge has NO HTTP auth in alpha. \
             Put a reverse proxy with auth (mTLS / OAuth / basic) in front before exposing \
             beyond loopback. See docs/production-checklist.md."
        );
    }

    let app = Router::new()
        .route("/health", get(chat::health))
        .route("/chat", post(chat::chat))
        .route("/chat/stream", post(chat::chat_stream))
        .route("/ws/chat", get(ws::chat_ws))
        .route("/chat_with_tool", post(chat::chat_with_tool))
        .route("/v1/models", get(openai::models))
        .route("/v1/info", get(openai::info))
        .route("/v1/schema", get(schema::schema))
        .route("/v1/control-plane/spine", get(control_plane::spine))
        .route(
            "/v1/control-plane/dashboard",
            get(control_plane::dashboard_manifest),
        )
        .route("/v1/activity/recent", get(activity::recent))
        .route("/v1/workspaces", get(workspaces::list))
        .route("/v1/workspaces", post(workspaces::create))
        .route("/v1/workspaces/{lease_id}", get(workspaces::get))
        .route(
            "/v1/workspaces/{lease_id}/release",
            post(workspaces::release),
        )
        .route("/v1/sessions/export", get(export::export))
        .route("/v1/chat/completions", post(openai::chat_completions))
        // Task-native read API (Track 2). Bridge stays translation-only:
        // each route is a thin forwarder to a Coordinator capability.
        // `/spine` is RETIRED (Phase 2 Slice 2): the interim spine board HTML
        // is deleted and React (`/dashboard`) is the one product surface.
        // `/spine` now PERMANENTLY (308) redirects to `/dashboard`, kept only
        // so old bookmarks/docs resolve. The `/v1/spine/*` JSON routes below
        // are the real, supported product-spine API.
        .route("/spine", get(spine::page))
        .route("/v1/spine/companion", post(companion::handle))
        .route("/v1/spine/guild", get(spine::guild_counts))
        .route("/v1/spine/guild/detail", get(spine::guild_detail))
        .route("/v1/spine/guild/spend", get(spine::guild_spend))
        .route(
            "/v1/spine/allowance/committed",
            get(spine::allowance_committed),
        )
        .route("/v1/spine/board", get(spine::board_summary))
        .route("/v1/spine/board/:column", get(spine::board_column))
        .route("/v1/spine/roster", get(spine::roster_summary))
        // Agent adapters (Rigs): which local coding-agent CLIs are
        // registered + a live availability probe. Backs the dashboard
        // Settings + Crew adapter panels.
        .route("/v1/adapters", get(adapters::list))
        .route(
            "/v1/spine/mandates",
            get(spine::mandates).post(spine::create_mandate),
        )
        .route("/v1/spine/mandates/search", get(spine::mandate_search))
        .route("/v1/spine/mandates/:id/tree", get(spine::mandate_tree))
        .route("/v1/spine/mandates/:id/briefs", get(spine::mandate_briefs))
        .route(
            "/v1/spine/mandates/:id/team_plan",
            get(spine::team_plan_latest).post(spine::team_plan),
        )
        .route(
            "/v1/spine/mandates/:id/team_readiness",
            get(spine::team_readiness),
        )
        .route(
            "/v1/spine/mandates/:id/orchestrate",
            post(spine::orchestrate),
        )
        .route(
            "/v1/spine/mandates/:id/orchestration/latest",
            get(spine::orchestration_latest),
        )
        // Prime guided driver v1 (mandate entry): next governed step + one-step
        // advance (company-model §5.4/§8.2 + §12.5).
        .route(
            "/v1/spine/mandates/:id/next-step",
            get(spine::mandate_next_step),
        )
        .route(
            "/v1/spine/mandates/:id/advance",
            post(spine::mandate_advance),
        )
        .route(
            "/v1/spine/mandates/:id/strategy",
            get(spine::strategy_status),
        )
        .route(
            "/v1/spine/mandates/:id/strategy/propose",
            post(spine::strategy_propose),
        )
        .route(
            "/v1/spine/mandates/:id/strategy/approve",
            post(spine::strategy_approve),
        )
        .route(
            "/v1/spine/mandates/:id/strategy/reject",
            post(spine::strategy_reject),
        )
        .route("/v1/spine/briefs/search", get(spine::brief_search))
        .route("/v1/spine/briefs/:id", get(spine::brief_detail))
        .route("/v1/spine/briefs/:id/wakeups", get(spine::brief_wakeups))
        .route("/v1/spine/desk/:agent", get(spine::desk))
        .route("/v1/spine/by-label", get(spine::by_label))
        .route("/v1/spine/overdue", get(spine::overdue))
        .route("/v1/spine/blocked", get(spine::blocked))
        .route("/v1/spine/stale", get(spine::stale))
        .route("/v1/spine/unblocked", get(spine::unblocked))
        .route("/v1/spine/unassigned", get(spine::unassigned))
        // Operative Keys (read) + assign-Key preview + pending Clearances.
        .route("/v1/spine/keys/:agent", get(spine::keys))
        .route("/v1/spine/assign_check", get(spine::assign_check))
        .route("/v1/spine/clearances", get(spine::clearances))
        // The dedicated polling-backed Clearance stream. The static `stream`
        // segment sits beside the sibling `:approval_id/decide` param of the
        // decide route — axum/matchit gives the static path priority, so they
        // do not conflict (same shape as `…/interactions/stream` vs
        // `…/interactions/:iid`). Register before the param route for clarity.
        .route("/v1/spine/clearances/stream", get(spine::clearances_stream))
        .route(
            "/v1/spine/clearances/:approval_id/decide",
            post(spine::decide_clearance),
        )
        // Prime Assistant: governed "describe what you want → plan".
        .route("/v1/spine/prime/propose", post(spine::prime_propose))
        .route("/v1/spine/prime/approve", post(spine::prime_approve))
        .route("/v1/spine/prime/start", post(spine::prime_start))
        .route("/v1/spine/prime/proposals", get(spine::prime_proposals))
        .route("/v1/spine/prime/proposals/:id", get(spine::prime_proposal))
        // Read-only Prime standing-authority state (which approve categories are
        // active for the Guild). Grants are managed via the standing-approval routes.
        .route(
            "/v1/spine/prime/standing-authority",
            get(spine::prime_standing_authority),
        )
        // Prime Runtime Autonomy Switch (v1): turn the autonomous Prime loop
        // on/off for the Guild at runtime (GET state / PUT {enabled}). Persisted
        // tenant-scoped in the coordinator DB; role-gated; not an approval bypass.
        .route(
            "/v1/spine/prime/autonomy",
            get(spine::prime_autonomy).put(spine::prime_autonomy_set),
        )
        // Manual Autonomy Tick (v1): run exactly one bounded autonomous Prime
        // tick for the Guild on operator request and return the tick records.
        // Does NOT require the runtime switch ON; same governed gates as the
        // timer path. (Static `…/autonomy/tick` does not collide with `…/autonomy`.)
        .route(
            "/v1/spine/prime/autonomy/tick",
            post(spine::prime_autonomy_tick),
        )
        // Live Shift-Room status of a Prime work session: a dedicated
        // tenant-scoped SSE stream (preferred) + the polling snapshot (fallback).
        .route(
            "/v1/spine/prime/proposals/:id/status/stream",
            get(spine::prime_status_stream),
        )
        .route(
            "/v1/spine/prime/proposals/:id/status",
            get(spine::prime_status),
        )
        // Prime guided driver v1 (proposal entry): next governed step + one-step
        // advance. The advance maps a stale refusal to 409.
        .route(
            "/v1/spine/prime/proposals/:id/next-step",
            get(spine::prime_proposal_next_step),
        )
        .route(
            "/v1/spine/prime/proposals/:id/advance",
            post(spine::prime_proposal_advance),
        )
        // Composite Desk/Inbox + Brief live-thread payloads.
        .route("/v1/spine/inbox", get(spine::inbox))
        .route("/v1/spine/briefs/:id/events", get(spine::brief_events))
        .route("/v1/spine/briefs/:id/thread", get(spine::brief_thread))
        // Product-spine dashboard write actions.
        .route("/v1/spine/briefs", post(spine::create_brief))
        .route("/v1/spine/briefs/:id/move", post(spine::move_brief))
        // Run a Brief now through its Operative's agent adapter (Rig).
        .route("/v1/spine/briefs/:id/run", post(spine::run_brief))
        // Execution-run ledger: recent runs (Active Runs feed) + a
        // Brief's run/Shift history.
        .route("/v1/runs", get(spine::runs_recent))
        // Persisted adapter runtime state (TG2). Static paths registered
        // before `:run_id` so they take routing precedence.
        .route("/v1/runs/runtime-state", get(spine::runtime_state_get))
        .route(
            "/v1/runs/runtime-state/list",
            get(spine::runtime_state_list),
        )
        .route(
            "/v1/runs/runtime-state/reset",
            post(spine::runtime_state_reset),
        )
        // Run/Brief execution event stream (TG5) — tenant-scoped SSE.
        .route("/v1/runs/events/stream", get(tasks::runs_events_stream))
        // Active Runs snapshot stream (dashboard-design §10/§11): tenant-scoped
        // SSE that proxies `brief.runs` on a bounded poll and pushes only when
        // the recent-run ledger changes (fingerprint-gated), so the Active Runs
        // table refreshes as a run moves `running` → `done`/`failed` without a
        // manual reload. The static `stream` segment sits before `:run_id`
        // (axum/matchit gives the static path priority).
        .route("/v1/runs/stream", get(spine::runs_stream))
        .route("/v1/runs/:run_id", get(spine::run_get))
        .route("/v1/runs/:run_id/events", get(spine::run_events))
        .route("/v1/runs/:run_id/cancel", post(spine::run_cancel))
        .route("/v1/runs/:run_id/retry", post(spine::run_retry))
        .route("/v1/runs/:run_id/artifacts", get(spine::run_artifacts))
        .route(
            "/v1/runs/:run_id/artifacts/:artifact_id/preview",
            get(spine::run_artifact_preview),
        )
        .route(
            "/v1/runs/:run_id/artifacts/:artifact_id/diff",
            get(spine::run_artifact_diff),
        )
        .route("/v1/runs/:run_id/review", post(spine::run_review))
        .route("/v1/runs/:run_id/diff", get(spine::run_diff))
        .route("/v1/runs/:run_id/apply", post(spine::run_apply))
        .route("/v1/runs/:run_id/discard", post(spine::run_discard))
        .route("/v1/maintenance/summary", get(spine::maintenance_summary))
        .route("/v1/maintenance/prune", post(spine::maintenance_prune))
        .route("/v1/maintenance/audit", get(spine::maintenance_audit))
        .route("/v1/spine/briefs/:id/runs", get(spine::brief_runs))
        // Issue-tree cost rollup + billing attribution (company-model §6.6).
        .route("/v1/spine/briefs/:id/cost", get(spine::brief_cost_rollup))
        // First-run company bootstrap: status read, Crew roster, and the
        // owner-gated "Initialize Company" (create the Founder) action.
        .route("/v1/spine/company", get(spine::company_status))
        .route("/v1/spine/company/init", post(spine::company_init))
        // First-run safe-local on-ramp: Founder + echo starter crew (§12.6).
        .route(
            "/v1/spine/company/starter-crew",
            post(spine::company_starter_crew),
        )
        // Action Center: the operator's next-actions feed (company-model §8.2).
        .route("/v1/spine/company/actions", get(spine::company_actions))
        // Action Center snapshot stream (company-model §5.4/§8.2): tenant-scoped
        // SSE that proxies `company.actions` on a bounded poll and pushes only
        // when the feed changes (fingerprint-gated), so the Command Center
        // refreshes as approvals/hires/blockers/needs-review appear. The static
        // `stream` segment sits beside the sibling `actions` route (no conflict).
        .route(
            "/v1/spine/company/actions/stream",
            get(spine::company_actions_stream),
        )
        .route("/v1/spine/operatives", get(spine::operatives))
        // Run-workspace context config (mode / project root / caps).
        .route("/v1/spine/run-config", get(spine::run_config))
        .route("/v1/spine/briefs/:id/pin", post(spine::pin_brief))
        .route("/v1/spine/briefs/:id/comment", post(spine::comment_brief))
        // §1.9 thread interactions: list/raise answerable cards + answer one.
        .route(
            "/v1/spine/briefs/:id/interactions",
            get(spine::list_interactions).post(spine::open_interaction),
        )
        // Dedicated REALTIME interaction-card stream (dashboard-design §7/§11):
        // tenant-scoped SSE that proxies `brief.interactions` on a bounded poll
        // and pushes only when the card list changes (fingerprint-gated), so the
        // workroom's ask/confirm/suggest/plan-package cards refresh even when no
        // run event fires. The static `stream` segment sits beside the `:iid`
        // param of the respond route below (axum/matchit gives the static path
        // priority — same shape as `/v1/runs/events/stream` vs `/v1/runs/:id/…`).
        .route(
            "/v1/spine/briefs/:id/interactions/stream",
            get(spine::interactions_stream),
        )
        .route(
            "/v1/spine/briefs/:id/interactions/:iid/respond",
            post(spine::respond_interaction),
        )
        // §1.9: cancel an answerable card (close without answering).
        .route(
            "/v1/spine/briefs/:id/interactions/:iid/cancel",
            post(spine::cancel_interaction),
        )
        // §1.8 approval-bound plan confirm: open a `confirm` bound to the
        // Brief's latest `plan` Dossier revision (answered via the respond
        // route above; a stale accept after a newer plan / a comment expires).
        .route(
            "/v1/spine/briefs/:id/plan-confirm",
            post(spine::open_plan_confirm),
        )
        // §1.9 suggest_tasks: propose a child-Brief tree, accept (materialize
        // as Sub-briefs) or reject it.
        .route(
            "/v1/spine/briefs/:id/suggestions",
            post(spine::open_suggestion),
        )
        .route(
            "/v1/spine/briefs/:id/suggestions/:iid/respond",
            post(spine::respond_suggestion),
        )
        // §1.7/§1.8/§3.1 plan package: open a plan Dossier + suggest_tasks
        // proposal + approval-bound confirm linked to both, then accept/reject
        // the confirm. Accepting materializes the linked proposal through the
        // resumable, exactly-once decomposition ledger.
        .route(
            "/v1/spine/briefs/:id/plan-package",
            post(spine::open_plan_package),
        )
        .route(
            "/v1/spine/briefs/:id/plan-confirms/:cid/respond",
            post(spine::respond_plan_confirm),
        )
        // §1.8 issue-document authoring: author a Dossier revision with
        // optimistic locking / explicit fork (POST), and load the latest
        // revision of a kind with its body (GET). Both static `dossiers/*`
        // segments — no param conflict with the other `:id/...` routes.
        .route(
            "/v1/spine/briefs/:id/dossiers/author",
            post(spine::author_dossier),
        )
        .route(
            "/v1/spine/briefs/:id/dossiers/latest",
            get(spine::dossier_latest),
        )
        // §1.8 document locking: lock / unlock a logical Dossier (a Brief's
        // document `kind`) and list the active locks. Static `dossiers/*`
        // segments — no param conflict with the other `:id/...` routes.
        .route(
            "/v1/spine/briefs/:id/dossiers/lock",
            post(spine::lock_dossier),
        )
        .route(
            "/v1/spine/briefs/:id/dossiers/unlock",
            post(spine::unlock_dossier),
        )
        .route(
            "/v1/spine/briefs/:id/dossiers/locks",
            get(spine::list_dossier_locks),
        )
        .route("/v1/spine/briefs/:id/due", post(spine::set_due))
        .route("/v1/spine/briefs/:id/set", post(spine::set_field))
        .route("/v1/spine/briefs/:id/snag", post(spine::add_snag))
        .route("/v1/spine/briefs/:id/unsnag", post(spine::remove_snag))
        .route("/v1/spine/briefs/:id/subbrief", post(spine::add_subbrief))
        // Rig bridge-back surface. These routes are exempt from
        // the global bridge bearer in auth middleware, but each
        // handler validates its per-Shift `brt_*` token with the
        // coordinator before forwarding a narrow Brief-local call.
        .route(
            "/v1/bridge-back/briefs/:id/comment",
            post(bridge_back::comment),
        )
        .route(
            "/v1/bridge-back/briefs/:id/subbrief",
            post(bridge_back::subbrief),
        )
        .route(
            "/v1/bridge-back/briefs/:id/dossier",
            post(bridge_back::dossier),
        )
        .route(
            "/v1/bridge-back/briefs/:id/snags",
            post(bridge_back::set_snags),
        )
        .route(
            "/v1/bridge-back/briefs/:id/clearance",
            post(bridge_back::clearance),
        )
        .route(
            "/v1/bridge-back/briefs/:id/claim-holder",
            post(bridge_back::claim_holder),
        )
        .route("/v1/tasks", get(tasks::list))
        .route("/v1/tasks/count", get(tasks::count))
        .route("/v1/tasks/cursor", get(tasks::list_cursor))
        .route("/v1/tasks/:id", get(tasks::get_one))
        .route("/v1/tasks/:id/attempts", get(tasks::attempts))
        .route("/v1/tasks/:id/edges", get(tasks::edges))
        // M66: execution-lineage BFS from a root task. Walks
        // task_edges in both directions up to ?depth=N
        // (clamped to [1, 16]). Lives at a distinct path so
        // it doesn't collide with `/v1/tasks/:id/lineage`
        // (single-task lineage envelope, registered below).
        .route("/v1/tasks/:id/lineage_graph", get(tasks::lineage_graph))
        // Cross-task execution edges aggregate. Registered
        // before `/v1/tasks/:id` is matched — axum's static
        // matching prefers exact paths.
        .route("/v1/tasks/edges/recent", get(tasks::recent_edges))
        // M67: cross-task event firehose. Same static-prefix
        // discipline — register before /v1/tasks/:id paths.
        .route("/v1/tasks/events/recent", get(tasks::recent_events))
        // M73: long-lived SSE firehose. Same data source as
        // /events/recent, served as a stream so dashboards
        // get sub-second runtime visibility without polling.
        .route("/v1/tasks/events/stream", get(tasks::events_stream_global))
        // H6: stuck-running task projection. Read-only diagnostic
        // that surfaces tasks the recovery scan can't reach.
        // Register before /v1/tasks/:id paths so axum's static
        // matcher prefers it.
        .route("/v1/tasks/stuck", get(tasks::stuck))
        // PH-DASH2: per-task todo surface — read / replace /
        // update single item. Routes registered before the
        // catch-all /v1/tasks/:id paths so axum prefers them.
        .route("/v1/tasks/:id/todos", get(tasks::todo_list))
        .route("/v1/tasks/:id/todos", put(tasks::todo_put))
        .route("/v1/tasks/:id/todos/:todo_id", patch(tasks::todo_patch))
        .route("/v1/tasks/:id/summary", get(tasks::summary))
        .route("/v1/tasks/:id/events", get(tasks::events))
        // Experimental SSE wrapper around task.events polling.
        // Bridge-side polling; owns no per-stream task state.
        .route("/v1/tasks/:id/events/stream", get(tasks::events_stream))
        .route("/v1/tasks/:id/lineage", get(tasks::lineage))
        .route("/v1/tasks/:id/export", get(tasks::export))
        // Chronicle-retention Step 2: dry-run candidate counter.
        // Read-only (GET) because no deletion happens. The
        // destructive Step 3 mode will land as a separate POST
        // path with stricter guards (operator capability + body
        // confirmation), not as a query parameter here.
        .route(
            "/v1/tasks/compact_events",
            get(tasks::compact_events_dry_run),
        )
        .route("/v1/tasks/recover", post(tasks::recover))
        .route("/v1/tasks/:id/retry", post(tasks::retry))
        // W2-001c: operator-triggered replay. Clones the task
        // (preserves flow_template / params / retry-policy /
        // origin_surface; fresh retry_count) and wires a
        // retried_from edge from new → original.
        .route("/v1/tasks/:id/replay", post(tasks::replay))
        .route("/v1/tasks/:id/cancel", post(tasks::cancel))
        // Operator-authored annotation as a chronicle event
        // (M60). Body: {note: string}. The Coordinator records
        // the verified caller's subject_id as the note's
        // author; the bridge records the call in the
        // intervention audit ring too.
        .route("/v1/tasks/:id/note", post(tasks::note))
        // Operator-set investigation marker (M62). Body:
        // {marked: bool, reason?: string}. Toggles persistent
        // state on the task row + emits a chronicle event.
        .route("/v1/tasks/:id/investigation", post(tasks::investigation))
        // Operator pause / resume (M65). Pause transitions
        // pending|running|retrying → paused with a
        // task.paused chronicle event. Resume transitions
        // paused → pending. HONEST: no flow-pause primitive
        // exists yet — same caveat as cancel.
        .route("/v1/tasks/:id/pause", post(tasks::pause))
        .route("/v1/tasks/:id/resume", post(tasks::resume))
        // M71: operator freeze / unfreeze. Workflow-level
        // counterpart to pause. Status → frozen. Future
        // cooperative workers will observe + propagate the
        // freeze via M70 protocol.
        .route("/v1/tasks/:id/freeze", post(tasks::freeze))
        .route("/v1/tasks/:id/unfreeze", post(tasks::unfreeze))
        // T4 P2: capability discovery as JSON. Translation-only —
        // pure projection of the bridge's already-discovered
        // manifest cache (no extra mesh I/O).
        .route("/v1/capabilities", get(capabilities::list))
        .route("/v1/capabilities/:method", get(capabilities::get_one))
        // Multi-node operational realism: peer-level topology view
        // with freshness aggregates. Read-only projection of the
        // ManifestCache — no active probing, no orchestration
        // (bridge stays translation/presentation only).
        .route("/v1/topology", get(topology::get))
        // Server-side history of topology transitions (peer joins,
        // freshness flips, drops). Populated by the lifecycle diff
        // task that runs every 5s; in-memory ring; resets on
        // bridge restart.
        .route("/v1/topology/events", get(topology::lifecycle_events))
        // Per-stream tracking for /v1/tasks/:id/events/stream
        // consumers. Lists active streams + opened_at + age so
        // operators see "which task is being watched right now."
        .route("/v1/streams", get(topology::streams_list))
        // Routing snapshot: for each capability method known
        // to the bridge, the peer it would dispatch to right
        // now under the first-match-in-cache policy. Pure
        // projection of the manifest cache.
        .route("/v1/routing", get(topology::routing_snapshot))
        // PH-BRIDGE-MCP: HTTP proxy for the MCP registry on a
        // tool peer. Pure translation — dispatches via the
        // existing MeshClient and parses the tab-delim response
        // into JSON for dashboard / curl consumption.
        .route("/v1/mcp/servers", get(mcp::servers))
        .route("/v1/mcp/tools", get(mcp::tools))
        // PH-BRIDGE-MCP-INVOKE: invocation proxy. Honest about
        // D-009: returns 502 with `RuntimeNotConnected` until
        // the stdio runtime ships, but the surface is ready.
        .route("/v1/mcp/invoke", post(mcp::invoke))
        // PH-BRIDGE-MCP-AUDIT: bounded in-memory audit ring of
        // every dispatched `/v1/mcp/invoke` call. Resets on
        // bridge restart. Honest about scope: argument-validation
        // rejects (400) are not recorded; only invocations that
        // reached the mesh (success or responder failure) are.
        .route("/v1/mcp/audit", get(mcp::audit))
        // PH-BRIDGE-FS-AUDIT: proxy for `tool.fs.audit_recent`
        // on the tool peer. Returns the runtime-side mutation
        // ring as JSON. Query: `?peer=&max=&op=`.
        .route("/v1/fs/audit", get(fs_audit::audit))
        // PH-BRIDGE-TERM-AUDIT: proxy for `tool.terminal.audit_recent`
        // on the tool peer. Returns the runtime-side completion
        // ring as JSON. Query: `?peer=&max=`. The runtime
        // intentionally drops args from the audit body — only
        // the command is shipped.
        .route("/v1/terminal/audit", get(term_audit::audit))
        // PH-DASH-BLOCKLIST: proxy for `tool.web.blocklist_summary`
        // on the tool peer. Returns the operator-curated host
        // blocklist as JSON. Read-only — to change it, edit
        // `[tool] blocked_hosts` and restart the tool node.
        .route("/v1/tool/blocklist", get(blocklist::blocklist))
        // PH-DASH-BROWSER: proxy for `tool.browser.list_sessions`
        // on the tool peer. Returns the currently-open browser
        // sessions as JSON (session_id, opened_at, current_url,
        // status). Read-only — open/close go through the
        // existing libp2p dispatch.
        .route("/v1/browser/sessions", get(browser_sessions::sessions))
        // W2-006c: proxy for `node.dispatch.stats` on any peer.
        // Returns per-capability invocation + latency counters
        // (lifetime, reset on peer restart). Read-only.
        .route("/v1/dispatch/stats", get(dispatch_stats::stats))
        // W2-007b: proxy for `node.policy.simulate`. Operators
        // ask "what would the policy decide if a caller with
        // groups Y called method M?" without invoking M.
        .route("/v1/policy/simulate", get(policy_simulate::simulate))
        // W2-007e: proxy for `node.policy.recent_denials`.
        // Returns the bounded ring of recent policy-denied
        // attempts (capacity 256 per peer). Pure read.
        .route("/v1/policy/denials", get(policy_denials::denials))
        // GAP 23B: per-tenant policy enumeration + inspection.
        // Both endpoints are read-only proxies for the
        // `node.policy.tenant_*` caps on the target peer.
        .route("/v1/policy/tenants", get(policy_tenants::list_tenants))
        .route(
            "/v1/policy/tenants/:tenant_id",
            get(policy_tenants::get_tenant),
        )
        // GAP 23C: per-tenant audit enumeration + recent rows.
        // Read-only proxies for the `node.audit.tenant_*` caps.
        .route("/v1/audit/tenants", get(audit_tenants::list_tenants))
        .route("/v1/audit/tenants/:tenant_id", get(audit_tenants::recent))
        // W2-MEMORY-3: proxy for `memory.agent_read`. Returns
        // the persistent agent + user memory for a subject_id.
        // Read-only — writes happen via the agent's own
        // `memory` tool inside ai.chat sessions, never via the
        // dashboard.
        .route("/v1/memory/agent", get(agent_memory::agent_memory))
        // W2-MEMORY-CURATOR-3: manual curator trigger +
        // scheduler-status read.
        .route("/v1/memory/curate", post(memory_curator::curate))
        .route("/v1/memory/curator/status", get(memory_curator::status))
        .route("/v1/memory/embed", post(memory_embed::embed))
        .route("/v1/memory/search", post(memory_embed::search))
        .route("/v1/memory/sessions/search", get(session_search::search))
        .route("/v1/memory/embed_all", post(memory_embed::embed_all))
        // RELIX-7.15 PII: memory-layer scan + preview + migration.
        .route("/v1/memory/pii/scan", post(memory_pii::scan))
        .route("/v1/memory/pii/preview", post(memory_pii::preview))
        .route(
            "/v1/memory/pii/bulk_anonymize",
            post(memory_pii::bulk_anonymize),
        )
        // GAP 5: four missing memory caps — dialectic Q&A, doc /
        // image ingest, explicit context flush.
        .route("/v1/memory/dialectic", post(memory_gap5::dialectic))
        .route("/v1/memory/ingest", post(memory_gap5::ingest))
        .route("/v1/memory/ingest_image", post(memory_gap5::ingest_image))
        .route("/v1/memory/context_flush", post(memory_gap5::context_flush))
        // GAP 6: quarantine list / approve / reject.
        .route(
            "/v1/memory/quarantine/list",
            post(memory_gap5::quarantine_list),
        )
        .route(
            "/v1/memory/quarantine/approve",
            post(memory_gap5::quarantine_approve),
        )
        .route(
            "/v1/memory/quarantine/reject",
            post(memory_gap5::quarantine_reject),
        )
        // GAP 7: memory inspector editing surface.
        .route("/v1/memory/records/edit", post(memory_gap5::edit_record))
        .route(
            "/v1/memory/records/freeze",
            post(memory_gap5::freeze_record),
        )
        .route(
            "/v1/memory/records/unfreeze",
            post(memory_gap5::unfreeze_record),
        )
        .route("/v1/memory/export", post(memory_gap5::bulk_export))
        .route(
            "/v1/memory/refresh_model",
            post(memory_gap5::request_model_refresh),
        )
        // GAP 4: SkillStore HTTP surface.
        .route("/v1/skills", get(skills::list).post(skills::create))
        .route("/v1/skills/stats", get(skills::stats))
        .route("/v1/skills/:id", get(skills::get).patch(skills::update))
        .route("/v1/skills/:id/deprecate", post(skills::deprecate))
        // RELIX-7.16 knowledge transfer.
        .route("/v1/knowledge/share", post(knowledge::share))
        .route("/v1/knowledge/shared/:agent", get(knowledge::list_shared))
        .route("/v1/knowledge/broadcast", post(knowledge::broadcast))
        .route("/v1/knowledge/groups", get(knowledge::groups))
        .route("/v1/knowledge/revoke", post(knowledge::revoke))
        // RELIX-7.19: confidence scoring + fallback surface.
        .route("/v1/confidence/policies", get(confidence::policies))
        .route("/v1/confidence/history/:agent", get(confidence::history))
        .route("/v1/confidence/reset", post(confidence::reset))
        // RELIX-7.29 PART 1: smart-routing dry-run surface.
        .route("/v1/routing/explain", post(routing::explain))
        // DEFERRED C: operator-facing read for one approval —
        // calls `coord.approval.get` and returns the full JSON
        // row. Distinct from `/v1/approval/:id/delivery`
        // (which calls the §7.30 delivery store). HTTP 404
        // when the approval id is unknown.
        .route("/v1/approval/:id", get(approval::get_approval))
        // RELIX-7.30 PART 1: out-of-band approval delivery status.
        .route("/v1/approval/:id/delivery", get(approval::delivery_status))
        // PART 6: rows the dispatcher failed to deliver — operators
        // reconcile these via the dashboard before retrying.
        .route(
            "/v1/approval/failed-deliveries",
            get(approval::failed_deliveries),
        )
        // PART 5: dashboard surface — list every approval in
        // `pending` status so the dashboard UI can render
        // operator-facing approve / deny cards.
        .route("/v1/approval/pending", get(approval::pending_list))
        // PART 5: dashboard / CLI vote endpoint. Body carries
        // `{decision, note}`; the coordinator validates the
        // decision set and cancels any escalation timer for the
        // approval id atomically (PART 7).
        .route("/v1/approval/:id/decision", post(approval::record_decision))
        // PART 2: inbound Slack interactivity webhook. Verifies
        // `x-slack-signature` HMAC against the signing secret
        // (`RELIX_BRIDGE_SLACK_SIGNING_SECRET`), parses the
        // Block Kit `block_actions` payload, and forwards the
        // lifted decision to `approval.record_decision`.
        .route(
            "/v1/channels/slack/interact",
            post(channels::slack_interact),
        )
        // Slack FIX 2: inbound Slack Events API receiver.
        // Same signature-verification gate as /interact (shared
        // verifier in channels.rs). Handles `url_verification`
        // (challenge echo) and `event_callback` (ack 200,
        // process async). Operators paste this URL into Event
        // Subscriptions in the Slack app config.
        .route("/v1/channels/slack/events", post(channels::slack_events))
        // Telegram FIX 1: inbound Telegram webhook receiver.
        // Verifies the source IP against Telegram's published
        // ranges (149.154.160.0/20 + 91.108.4.0/22), parses
        // the Update payload, forwards to the Telegram peer via
        // mesh `telegram.webhook_update`, and responds HTTP 200
        // within Telegram's 5s budget. The bridge passes
        // ConnectInfo<SocketAddr> through via
        // `into_make_service_with_connect_info` at the bottom
        // of `main()`.
        .route(
            "/v1/channels/telegram/webhook",
            post(channels::telegram_webhook),
        )
        // PART 3: inbound Discord interactions endpoint.
        // Verifies the `X-Signature-Ed25519` +
        // `X-Signature-Timestamp` pair against the application
        // public key (`RELIX_BRIDGE_DISCORD_PUBLIC_KEY`),
        // PONGs the verification PING, and routes
        // MESSAGE_COMPONENT clicks to `approval.record_decision`.
        .route(
            "/v1/channels/discord/interact",
            post(channels::discord_interact),
        )
        // PART 4: inbound email reply webhook (Mailgun /
        // SendGrid / Postmark). Detects provider from the
        // body shape, HMAC-verifies Mailgun against
        // `RELIX_BRIDGE_MAILGUN_SIGNING_KEY`, parses the
        // operator's `APPROVE` / `DENY` subject token, and
        // routes to `approval.record_decision`.
        .route("/v1/channels/email/reply", post(channels::email_reply))
        // RELIX-7.30 PART 2: credential vault.
        .route(
            "/v1/credentials",
            post(credentials::store).get(credentials::list),
        )
        .route("/v1/credentials/:name", get(credentials::get))
        .route("/v1/credentials/:name/rotate", post(credentials::rotate))
        .route("/v1/credentials/:name/revoke", post(credentials::revoke))
        .route("/v1/credentials/:name/audit", get(credentials::audit))
        // RELIX-7.30 PART 3: session-identity tokens.
        .route(
            "/v1/identity/tokens",
            post(identity_session::issue).get(identity_session::list),
        )
        .route("/v1/identity/tokens/verify", post(identity_session::verify))
        .route("/v1/identity/tokens/revoke", post(identity_session::revoke))
        // RELIX-7.18 / GAP 17 PART 2: research-backed identity.
        .route("/v1/identity/research", post(identity_session::research))
        // RELIX-7.29 PART 3: belief-tracker inspection +
        // reset surface.
        .route("/v1/belief/:session_id", get(belief::get))
        .route("/v1/belief/:session_id", post(belief::post))
        // RELIX-7.29 PART 4: judge verdict + stats surface.
        .route("/v1/judge/verdicts", get(judge::verdicts))
        .route("/v1/judge/stats", get(judge::stats))
        // RELIX-7.29 PART 5: full reasoning-engine status.
        .route("/v1/reasoning/status", get(reasoning::status))
        // RELIX-7.24: spec-driven multi-agent planning surface.
        .route("/v1/planning/plan", post(planning::create_plan))
        .route("/v1/planning/agents", get(planning::list_agents))
        .route("/v1/planning/agents/search", post(planning::search_agents))
        .route("/v1/planning/validate", post(planning::validate_spec))
        .route("/v1/planning/status", get(planning::orchestrator_status))
        .route("/v1/planning/approve", post(planning::approve_plan))
        .route("/v1/planning/reject", post(planning::reject_plan))
        .route("/v1/planning/approvals", get(planning::list_approvals))
        .route("/v1/planning/approvals/:id", get(planning::get_approval))
        .route(
            "/v1/planning/verification/:id",
            get(planning::verification_log),
        )
        .route(
            "/v1/planning/verification/:id/stream",
            get(planning::verification_stream),
        )
        .route("/v1/planning/export/:id", get(planning::export_spec))
        .route("/v1/knowledge/recall", post(knowledge::recall))
        // Four-layer memory inspector. Reads the layered store
        // directly from `AppState::layered_memory` — set
        // `[bridge] memory_db_path` to enable.
        .route("/v1/memory/records", get(memory_inspect::list))
        .route("/v1/memory/records/{id}", get(memory_inspect::show))
        .route("/v1/memory/records/search", post(memory_inspect::search))
        .route(
            "/v1/memory/records/{id}/invalidate",
            post(memory_inspect::invalidate),
        )
        .route("/v1/memory/stats", get(memory_inspect::stats))
        // Multi-agent handoff audit ring.
        .route(
            "/v1/guardrails/handoffs",
            get(guardrails::handoffs).post(guardrails::record),
        )
        // JIT secret store inventory (NAMES ONLY).
        .route("/v1/secrets/available", get(secrets_available::available))
        // Two-sink session debugger surfaces.
        .route("/v1/sessions", get(sessions_obs::list))
        .route("/v1/sessions/{id}", get(sessions_obs::show))
        .route(
            "/v1/sessions/{session_id}/content/{event_id}",
            get(sessions_obs::content),
        )
        // Provenance registry — recorded surface per trace
        // plus a flat diff between two traces.
        .route("/v1/provenance/diff", get(provenance::diff))
        .route("/v1/provenance/recent", get(provenance::recent))
        .route("/v1/provenance/{trace_id}", get(provenance::show))
        // GAP 11 + 12: transactional gateway + evidence.
        .route("/v1/execution/rollback", post(execution::rollback))
        .route(
            "/v1/execution/transactions/{id}",
            get(execution::transaction_get),
        )
        .route("/v1/execution/evidence", get(execution::evidence))
        // Agent access policies + recent call counts.
        .route("/v1/agents/access", get(agents_access::agents))
        // Tool registry — list + keyword search.
        .route("/v1/tools", get(tools::list))
        .route("/v1/tools/search", post(tools::search))
        // Signed tool manifest (read-only).
        .route("/v1/tools/manifest", get(tools::manifest))
        // GAP 10 PART 3: tool.screen — proxy to the tool peer.
        .route("/v1/tools/screen", post(tool_screen::capture))
        // PH-TG-BRIDGE: proxy reads of the telegram channel
        // node. The bridge does not stand up its own bot
        // client; both routes call the telegram peer's
        // read-only capabilities (telegram.status,
        // telegram.messages_recent) and return parsed JSON.
        .route("/v1/discord/status", get(discord::status))
        .route("/v1/discord/messages/recent", get(discord::messages_recent))
        .route("/v1/slack/status", get(slack::status))
        .route("/v1/slack/messages/recent", get(slack::messages_recent))
        // RELIX-7.7: email-channel surface. Send + status are
        // mutating; the bridge mints a JSON envelope and ships
        // it to the configured email peer (default alias
        // `email`).
        .route("/v1/email/send", post(email::send))
        .route("/v1/email/send_template", post(email::send_template))
        .route("/v1/email/status", get(email::status))
        // RELIX-GAP-9: recent inbound messages (dashboard tile).
        .route("/v1/email/messages/recent", get(email::messages_recent))
        // RELIX-7.11: agent performance metrics. All six routes
        // proxy onto the coordinator's `metrics.*` capabilities.
        .route("/v1/metrics/agents", get(agent_metrics::list_agents))
        .route(
            "/v1/metrics/agents/:agent/summary",
            get(agent_metrics::agent_summary),
        )
        .route(
            "/v1/metrics/agents/:agent/methods",
            get(agent_metrics::agent_methods),
        )
        .route(
            "/v1/metrics/agents/:agent/timeseries",
            get(agent_metrics::agent_timeseries),
        )
        .route("/v1/metrics/alerts", get(agent_metrics::alerts))
        .route("/v1/metrics/cost", get(agent_metrics::cost))
        // GAP 22 Feature 2 follow-up: persisted baselines.
        .route(
            "/v1/metrics/cost-baselines",
            get(agent_metrics::cost_baselines),
        )
        .route(
            "/v1/metrics/ask-human-baselines",
            get(agent_metrics::ask_human_baselines),
        )
        .route("/v1/metrics/cost-spikes", get(agent_metrics::cost_spikes))
        // RELIX-7.28 Part 2: observability dashboard surface.
        .route(
            "/v1/observability/alerts",
            get(observability::active_alerts),
        )
        .route(
            "/v1/observability/alerts/history",
            get(observability::alert_history),
        )
        .route("/v1/observability/health", get(observability::health))
        // RELIX-7.28 Part 1: budget surface.
        .route("/v1/budget/status", get(budget::status))
        .route("/v1/budget/reset", post(budget::reset))
        // RELIX-7.28 Part 3: PII detection surface.
        .route("/v1/pii/stats", get(pii::stats))
        .route("/v1/pii/events", get(pii::events))
        // RELIX-7.15: training data pipeline. Six routes onto
        // the coordinator's `training.*` capabilities.
        .route(
            "/v1/training/interactions",
            get(training::list_interactions),
        )
        .route(
            "/v1/training/interactions/:id",
            get(training::get_interaction).delete(training::delete_interaction),
        )
        .route("/v1/training/export", post(training::export))
        .route("/v1/training/score/:id", post(training::score_interaction))
        .route("/v1/training/stats", get(training::stats))
        // RELIX-7.15 PII: scan + preview endpoints.
        .route("/v1/training/pii/scan", post(training::pii_scan))
        .route("/v1/training/pii/preview", post(training::pii_preview))
        .route("/v1/plugins", get(plugins::list))
        .route("/v1/plugins/:plugin_id", get(plugins::status))
        .route("/v1/plugins/:plugin_id/reload", post(plugins::reload))
        .route("/v1/plugins/:plugin_id/disable", post(plugins::disable))
        .route("/v1/telegram/status", get(telegram::status))
        .route(
            "/v1/telegram/messages/recent",
            get(telegram::messages_recent),
        )
        // PH-CRON-BRIDGE: cron scheduler. Six proxies onto the
        // coordinator's `cron.*` capabilities. List + create on
        // the collection; get / patch / delete / trigger on each
        // job.
        .route("/v1/cron/jobs", get(cron::list).post(cron::create))
        .route(
            "/v1/cron/jobs/:job_id",
            get(cron::get_one).patch(cron::update).delete(cron::delete),
        )
        .route("/v1/cron/jobs/:job_id/trigger", post(cron::trigger))
        // Workflow engine (RELIX-7.5). Five proxies onto the
        // coordinator's `workflow.*` capabilities. POST /run
        // executes by name — returns the full execution record,
        // or a live `text/event-stream` of per-step events when
        // the body sets `stream: true`. POST /reload drops the
        // coordinator's workflow file cache so in-place edits
        // pick up without a restart.
        .route("/v1/workflows", get(workflows::list))
        .route("/v1/workflows/run", post(workflows::run))
        .route("/v1/workflows/validate", post(workflows::validate))
        .route("/v1/workflows/reload", post(workflows::reload))
        .route("/v1/workflows/status/:execution_id", get(workflows::status))
        // PH-DELEGATE-BRIDGE: delegation surface. Four proxies onto
        // the coordinator's `delegate.*` capabilities. POST /spawn
        // creates the child task; GET /result polls its state;
        // POST /cancel terminates it; GET /list enumerates a
        // parent's children.
        .route("/v1/delegate/spawn", post(delegate::spawn))
        .route("/v1/delegate/result/:child_task_id", get(delegate::result))
        .route("/v1/delegate/cancel/:child_task_id", post(delegate::cancel))
        .route("/v1/delegate/list/:parent_task_id", get(delegate::list))
        // PH-AGENT-BRIDGE / REL-20: agent identity REST API.
        // CRUD + token-issuance + approval-decide + standing-approvals
        // proxies onto the coordinator's `agent.*` / `coord.approval.*`
        // / `identity.*` / `agent.standing_approval.*` capabilities.
        //
        // Token-issuance route registered before the `:agent_id`
        // catch-all so axum's static matcher prefers it.
        .route(
            "/v1/agents",
            get(agent::list_agents).post(agent::create_agent),
        )
        .route(
            "/v1/agents/:agent_id/tokens",
            post(agent::issue_agent_token),
        )
        .route(
            "/v1/agents/:agent_id",
            get(agent::get_agent)
                .patch(agent::update_agent)
                .delete(agent::delete_agent),
        )
        .route(
            "/v1/agents/:agent_id/approve-hire",
            post(agent::approve_hire),
        )
        .route("/v1/agents/:agent_id/reject-hire", post(agent::reject_hire))
        .route("/v1/approvals", get(agent::pending_approvals))
        .route(
            "/v1/approvals/:approval_id/decide",
            post(agent::decide_approval),
        )
        .route(
            "/v1/agents/:agent_id/standing-approvals",
            get(agent::list_standing).post(agent::create_standing),
        )
        .route(
            "/v1/standing-approvals/:standing_id",
            axum::routing::delete(agent::revoke_standing),
        )
        // PH-MSG-BRIDGE: agent-to-agent messaging. Five
        // proxies onto the coordinator's `msg.*` capabilities.
        .route("/v1/messages", post(messaging::send))
        .route("/v1/messages/inbox/:subject_id", get(messaging::inbox))
        .route("/v1/messages/:message_id/read", post(messaging::read))
        .route("/v1/messages/thread/:thread_id", get(messaging::thread))
        .route(
            "/v1/messages/:message_id",
            axum::routing::delete(messaging::delete),
        )
        // SOL/Sflow parse-only validator. Dashboard editors call this
        // to surface line-numbered errors inline before a flow is
        // deployed. No execution happens here — pure parse.
        .route("/v1/sol/validate", post(sol_validate::validate))
        // YAML twin of the SOL validator. Same parse-only
        // contract, distinct response shape — see
        // `crate::yaml_validate` for the JSON schema.
        .route("/v1/yaml/validate", post(yaml_validate::validate))
        // W2-002g: proxy for `tool.browser.capture_read`. Streams
        // a failure-screenshot PNG from the configured tool-peer
        // `screenshot_on_failure_dir` back to the dashboard with
        // `Content-Type: image/png`. Filename validation mirrors
        // the runtime; bad paths get 400 without crossing the mesh.
        .route(
            "/v1/browser/captures/:filename",
            get(browser_captures::capture),
        )
        // JSON-shaped health summary: uptime + coordinator status
        // + per-bucket peer counts + reconnect telemetry.
        // Distinct from /health (plaintext liveness probe).
        .route("/v1/health", get(topology::health))
        // Dashboard-facing config endpoints. Local/dev only —
        // no auth at the HTTP layer; production deployments
        // must put a reverse proxy with auth in front before
        // exposing the bridge beyond loopback. Secrets are
        // never echoed back; the bridge persists them to a
        // gitignored TOML file at mode 0600. See
        // docs/dashboard-redesign.md for the contract.
        .route("/v1/config", get(config_api::get_effective_config))
        // SEC PART 4: every provider-key-handling route is
        // removed. The bridge no longer holds, reads, or
        // dials AI provider API keys — operators configure
        // them in the AI node's own TOML and the AI node
        // performs every LLM call. The dashboard's "providers"
        // view is now a read-only enumeration of allowed
        // names + the operator-marked default. There is no
        // `PUT key` / `POST test` / `PUT quarantine` /
        // `PUT enabled` surface; the corresponding handlers,
        // dial helpers, and unbounded `resp.text()` paths
        // are deleted from config_api.rs.
        .route("/v1/config/providers", get(config_api::list_providers))
        .route("/v1/config/providers/:name", get(config_api::get_provider))
        .route(
            "/v1/config/providers/default",
            axum::routing::put(config_api::put_default_provider),
        )
        .route(
            "/v1/config/telegram",
            get(config_api::get_telegram).put(config_api::put_telegram),
        )
        .route("/v1/config/telegram/test", post(config_api::test_telegram))
        // Operator intervention audit ring (M57). Newest-first
        // record of every mutating operator-facing call:
        // retry, recover, cancel, provider CRUD/test, telegram
        // save/test. JSONL on disk under data_dir +
        // in-memory ring (resets on restart).
        .route("/v1/intervention/recent", get(intervention_audit::recent))
        // Dashboard Section 18: real-time log tail. SSE stream
        // that ships the last 500 lines from the in-memory ring
        // first, then live-tails the broadcast channel
        // populated by the LogRingLayer installed on tracing
        // init in `main()`. Keep-alive comments every 15s so
        // idle connections survive reverse-proxy timeouts.
        .route("/v1/logs/stream", get(logs::stream))
        // Operator dashboard. The React SPA (apps/dashboard, built to
        // crates/relix-web-bridge/dashboard-dist) served at /dashboard with
        // its assets under /dashboard/assets/*. Phase 2 Slice 3: the legacy
        // single-file HTML dashboard is RETIRED — there is no HTML fallback.
        // If the bundle is missing, /dashboard returns an honest 503
        // missing-bundle notice (run `npm run build`), never an old console.
        // /dashboard + /dashboard/* are public-allowlisted in the auth
        // middleware so an operator can reach the login screen.
        .merge(dashboard::dashboard_router())
        // One-time bootstrap so the dashboard can pick up its
        // bearer token without the operator pasting it manually.
        // Guarded inside the handler: refuses when the caller
        // already has an Authorization header, and CSRF-checks
        // the Origin. See `auth.rs`.
        .route("/v1/auth/token", get(auth::bootstrap_token))
        // Dashboard operator login (username/password + Argon2id, HTTP-
        // only session cookie). These are public (allowlisted in the auth
        // middleware) so an operator can reach the login screen before
        // they hold any credential; each endpoint self-gates.
        .route("/v1/auth/status", get(dashboard_auth::status))
        .route("/v1/auth/setup", post(dashboard_auth::setup))
        .route("/v1/auth/login", post(dashboard_auth::login))
        .route("/v1/auth/logout", post(dashboard_auth::logout))
        .route("/v1/auth/me", get(dashboard_auth::me))
        // Per-principal rate-limit middleware. Runs AFTER auth so
        // each principal gets its own bucket. Layered below the
        // auth layer in the source — axum applies layers
        // outermost-last, so the topology is:
        //   incoming request -> auth -> rate_limit -> handler
        // (auth runs first; on success the request passes through
        // the rate limiter; on overflow the limiter returns 429).
        .layer(axum::middleware::from_fn_with_state(
            state.rate_limits.clone(),
            rate_limit::rate_limit_middleware,
        ))
        // Token + CSRF guard for every mutating route. Public
        // routes (/health, /dashboard, /v1/auth/token, /assets/*)
        // are allowlisted inside the middleware itself. The
        // OpenAI shim is auth-special — any non-empty bearer
        // wins because OpenAI clients always send one. See
        // `auth.rs` for the full policy.
        .layer(axum::middleware::from_fn_with_state(
            auth::AuthState {
                token: state.bridge_token.clone(),
                host: state.bridge_host.clone(),
                port: state.bridge_port,
                // PART 8: bearer prefixes the auth layer
                // admits alongside the canonical bridge_token.
                // Populated from `[auth.tenant_bindings]`; the
                // tenant middleware (mounted underneath) reads
                // the same prefixes to resolve the per-request
                // tenant id.
                tenant_binding_prefixes: state
                    .cfg
                    .auth
                    .tenant_bindings
                    .keys()
                    .map(|s| s.to_lowercase())
                    .collect(),
                // Admit logged-in dashboard requests that ride the
                // `relix_session` cookie (operator login).
                dashboard_auth: Some(state.dashboard_auth.clone()),
            },
            auth::auth_middleware,
        ))
        // PART 5: per-request tenant identifier middleware.
        // Resolves the canonical tenant via the decision tree
        // in `crate::tenant::resolve_tenant`:
        //   1. authenticated bearer + binding in
        //      [auth.tenant_bindings] → use the binding.
        //   2. authenticated request with no binding +
        //      multi_tenant_mode → HTTP 401.
        //   3. trusted internal origin sending
        //      X-Relix-Tenant → accept the header.
        //   4. anything else → single-tenant default.
        // Stashes the resolved `TenantId` in request
        // Extensions so handlers + `peer_call::build_mesh_request`
        // can pull it out.
        .layer(axum::middleware::from_fn_with_state(
            tenant::TenantConfig::from_auth_section(&state.cfg.auth),
            tenant::tenant_middleware,
        ))
        // Universal security headers (CSP, X-Frame-Options,
        // X-Content-Type-Options). Layered outermost so the
        // headers ride 401/403/429 responses from the inner
        // layers too. See `crate::security_headers`.
        .layer(axum::middleware::from_fn(
            security_headers::security_headers_middleware,
        ))
        .with_state(state)
        // H15: per-request latency tracing. Emits one structured
        // `bridge: route` info line per request with method, path,
        // status, and elapsed_ms. No in-process state — operators
        // get p50/p95 via log scrape. Layered last so it wraps
        // every route registered above.
        .layer(axum::middleware::from_fn(route_latency_log));

    // Bind the HTTP listener FIRST — before the "web bridge starting"
    // log and the token banner. If the port is already held (typically
    // a stale bridge from an earlier boot), fail loudly with an
    // actionable message instead of letting the stale instance keep
    // serving while this boot prints a fresh, MISMATCHED token. Binding
    // before the success needle also means a collision never emits
    // "web bridge starting", so the boot scripts report the failure
    // rather than being fooled by the stale instance answering /health.
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            return Err(format!(
                "a Relix bridge (or another process) is already listening on {addr}; \
                 stop it first (`relix-mesh-down`, or `relix stop`) — otherwise the stale \
                 instance shadows this boot and keeps serving with a different setup token"
            )
            .into());
        }
        Err(e) => return Err(format!("bind {addr}: {e}").into()),
    };
    tracing::info!(
        listen = %addr,
        flow_template = %cfg.flow.template_path.display(),
        peers = %cfg.transport.peers_path.display(),
        openai_compat = cfg.openai_compat.is_some(),
        sse_chunk_bytes = cfg.sse.chunk_bytes,
        "web bridge starting"
    );
    // Token + dashboard banner. Goes via println! (not tracing)
    // so operators see it even when RUST_LOG silences the
    // bridge crate. The token line is what an operator pastes
    // into a curl `-H "Authorization: Bearer ..."` invocation.
    println!(
        "Bridge token: {}  (stored in {})",
        bridge_token_value,
        bridge_token_path.display()
    );
    println!("Dashboard:    http://{}/dashboard", addr);
    // Telegram FIX 1: `into_make_service_with_connect_info::<SocketAddr>()`
    // populates the `ConnectInfo<SocketAddr>` extractor on
    // every request, used by the `/v1/channels/telegram/webhook`
    // route to verify the source IP is in Telegram's published
    // ranges. Every other route is unaffected — they don't
    // declare a `ConnectInfo` extractor.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
