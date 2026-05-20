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
    routing::{get, post},
};
use clap::Parser;

mod capabilities;
mod chat;
mod config;
mod config_api;
mod dashboard;
mod flow;
mod lifecycle;
mod metrics;
mod openai;
mod secrets;
mod sse;
mod task_recorder;
mod tasks;
mod topology;
mod validate;

use crate::config::{AppState, BridgeConfig};

#[derive(Parser, Debug)]
#[command(
    name = "relix-web-bridge",
    version,
    about = "Local HTTP bridge that triggers a Relix SOL chat flow."
)]
struct Args {
    /// Path to the bridge config TOML (see `configs/web-bridge.toml`).
    #[arg(short, long)]
    config: PathBuf,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let cfg: BridgeConfig = {
        let text = std::fs::read_to_string(&args.config)
            .map_err(|e| format!("read config {}: {e}", args.config.display()))?;
        toml::from_str(&text).map_err(|e| format!("parse config: {e}"))?
    };
    let mut state = AppState::try_new(cfg.clone())?;

    // M10 + M11: discovery pass that *also* hands back a long-lived
    // MeshClient. The libp2p transport + dial cost is now paid once at
    // startup; every /chat thereafter reuses it.
    let discovery_opts = relix_runtime::manifest::DiscoveryOptions {
        identity_bundle: state.identity_bundle.clone(),
        client_key: state.client_key,
        peers: state.peers.clone(),
        deadline_secs: state.cfg.transport.deadline_secs.min(10),
        overall_timeout: std::time::Duration::from_secs(6),
        local_port: None,
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

    let app = Router::new()
        .route("/health", get(chat::health))
        .route("/chat", post(chat::chat))
        .route("/chat/stream", post(chat::chat_stream))
        .route("/chat_with_tool", post(chat::chat_with_tool))
        .route("/v1/models", get(openai::models))
        .route("/v1/chat/completions", post(openai::chat_completions))
        // Task-native read API (Track 2). Bridge stays translation-only:
        // each route is a thin forwarder to a Coordinator capability.
        .route("/v1/tasks", get(tasks::list))
        .route("/v1/tasks/count", get(tasks::count))
        .route("/v1/tasks/cursor", get(tasks::list_cursor))
        .route("/v1/tasks/:id", get(tasks::get_one))
        .route("/v1/tasks/:id/attempts", get(tasks::attempts))
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
        .route("/v1/tasks/:id/cancel", post(tasks::cancel))
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
        .route("/v1/config/providers", get(config_api::list_providers))
        .route(
            "/v1/config/providers/:name",
            get(config_api::get_provider)
                .put(config_api::put_provider)
                .delete(config_api::delete_provider),
        )
        .route(
            "/v1/config/providers/:name/test",
            post(config_api::test_provider),
        )
        .route(
            "/v1/config/providers/default",
            axum::routing::put(config_api::put_default_provider),
        )
        .route(
            "/v1/config/telegram",
            get(config_api::get_telegram).put(config_api::put_telegram),
        )
        .route("/v1/config/telegram/test", post(config_api::test_telegram))
        // Operator dashboard. Single-page static HTML; consumes
        // the existing /v1/tasks* endpoints. No server-side
        // state introduced. See docs/bridge-invariants.md.
        .route("/dashboard", get(dashboard::page))
        .with_state(state);

    tracing::info!(
        listen = %addr,
        flow_template = %cfg.flow.template_path.display(),
        peers = %cfg.transport.peers_path.display(),
        openai_compat = cfg.openai_compat.is_some(),
        sse_chunk_bytes = cfg.sse.chunk_bytes,
        "web bridge starting"
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
