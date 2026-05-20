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

mod chat;
mod config;
mod flow;
mod openai;
mod sse;
mod task_recorder;
mod tasks;
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
        .route("/v1/tasks/:id", get(tasks::get_one))
        .route("/v1/tasks/:id/attempts", get(tasks::attempts))
        .route("/v1/tasks/:id/summary", get(tasks::summary))
        .route("/v1/tasks/recover", post(tasks::recover))
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
