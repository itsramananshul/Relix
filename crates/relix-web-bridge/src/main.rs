//! relix-web-bridge — local HTTP bridge that turns `POST /chat` into a Relix
//! SOL chat orchestration on the mesh (M8).
//!
//! ## What the bridge is
//!
//! A normal Relix peer that happens to expose an HTTP server. It accepts a
//! tiny JSON request, materializes a per-request SOL flow file (substituting
//! the supplied `session_id` and `message` into a template), and asks the
//! existing `relix_runtime::flow_runner::FlowRunner` to execute it against
//! the configured peer aliases. The flow itself does the orchestration:
//! `memory.write_turn → memory.recent_for_session → ai.chat → memory.write_turn`.
//!
//! ## What the bridge is NOT
//!
//! - Not a central gateway. It calls the mesh as a normal peer identity;
//!   responders run the full admission pipeline (identity + policy + audit)
//!   on every call.
//! - Not the owner of any AI provider key. The Anthropic key (when used) lives
//!   only on the AI node. The bridge has no way to learn it.
//! - Not an orchestrator. The SOL flow file is the orchestration; the bridge
//!   only renders + runs it.
//!
//! ## Wire contract (alpha — SIMP-018)
//!
//! ```text
//! POST /chat
//! Content-Type: application/json
//! {
//!   "session_id": "demo-session",
//!   "message":    "hello"
//! }
//!
//! 200 OK
//! Content-Type: application/json
//! {
//!   "reply":     "<provider reply text>",
//!   "flow_id":   "<hex>",
//!   "trace_id":  "<hex>",
//!   "flow_log":  "<path>"
//! }
//!
//! GET /health → 200 OK  "ok\n"
//! ```
//!
//! SSE / chunked streaming is the next milestone (M8a). M8 ships the JSON
//! shape first because it cleanly proves the architectural seam.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use serde::{Deserialize, Serialize};

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::flow_runner::{FlowRunOptions, FlowRunner, FlowRunnerError, PeersFile};

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

/// Bridge config parsed from TOML.
#[derive(Clone, Debug, Deserialize)]
struct BridgeConfig {
    bridge: BridgeSection,
    identity: IdentitySection,
    transport: TransportSection,
    flow: FlowSection,
}

#[derive(Clone, Debug, Deserialize)]
struct BridgeSection {
    /// `127.0.0.1:9100` by default. Always loopback in alpha.
    listen_addr: String,
}

#[derive(Clone, Debug, Deserialize)]
struct IdentitySection {
    /// The signed IdentityBundle this bridge presents to mesh responders.
    bundle_path: PathBuf,
    /// 32-byte secret used as the local libp2p PeerId AND as the signer for
    /// the per-flow event log records.
    client_key_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
struct TransportSection {
    /// Path to the peer alias map (`relix_runtime::flow_runner::PeersFile`).
    peers_path: PathBuf,
    /// Per-call deadline. Default 30 s.
    #[serde(default = "default_deadline")]
    deadline_secs: i64,
    /// Per-flow event log directory; defaults to `RELIX_DATA_DIR` discovery.
    #[serde(default)]
    data_dir: Option<PathBuf>,
}

fn default_deadline() -> i64 {
    30
}

#[derive(Clone, Debug, Deserialize)]
struct FlowSection {
    /// Path to the SOL chat template file. Two placeholders are substituted
    /// at request time: `{{SESSION}}` and `{{MESSAGE}}`.
    template_path: PathBuf,
}

/// In-memory app state: validated config + preloaded identity bundle + peers.
#[derive(Clone)]
struct AppState {
    cfg: Arc<BridgeConfig>,
    identity_bundle: Bundle,
    client_key: [u8; 32],
    peers: PeersFile,
    template: String,
}

impl AppState {
    fn try_new(cfg: BridgeConfig) -> Result<Self, BridgeError> {
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

        Ok(Self {
            cfg: Arc::new(cfg),
            identity_bundle,
            client_key,
            peers,
            template,
        })
    }
}

// ──────────────────────────── HTTP handlers ────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatRequest {
    session_id: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    reply: String,
    flow_id: String,
    trace_id: String,
    flow_log: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    flow_id: Option<String>,
    flow_log: Option<String>,
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

async fn chat(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Validate the inputs against SIMP-018: SOL strings have no escape syntax,
    // so the only safe path is to forbid characters that would break out of
    // the literal or the SIMP-016 `|` delimiter.
    if let Err(e) = validate_input(&req.session_id, &req.message) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e,
                flow_id: None,
                flow_log: None,
            }),
        ));
    }

    // Render the SOL flow with the request values substituted in.
    let rendered = state
        .template
        .replace("{{SESSION}}", &req.session_id)
        .replace("{{MESSAGE}}", &req.message);

    // Materialize to a tempfile so the existing flow runner consumes it.
    let tmp = tempfile::Builder::new()
        .prefix("relix-bridge-chat-")
        .suffix(".sol")
        .tempfile()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("tempfile: {e}"),
                    flow_id: None,
                    flow_log: None,
                }),
            )
        })?;
    if let Err(e) = std::fs::write(tmp.path(), rendered.as_bytes()) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("write tempfile: {e}"),
                flow_id: None,
                flow_log: None,
            }),
        ));
    }
    let flow_path = tmp.path().to_path_buf();

    // Run the flow through the existing real-libp2p runner.
    let opts = FlowRunOptions {
        flow_path,
        identity_bundle: state.identity_bundle.clone(),
        client_key: state.client_key,
        peers: state.peers.clone(),
        data_dir: state.cfg.transport.data_dir.clone(),
        deadline_secs: state.cfg.transport.deadline_secs,
    };

    match FlowRunner::new(opts).run().await {
        Ok(result) => {
            let reply = result.final_string.unwrap_or_default();
            Ok(Json(ChatResponse {
                reply,
                flow_id: result.flow_id.to_string(),
                trace_id: result.trace_id.to_string(),
                flow_log: result.flow_log_path.to_string_lossy().to_string(),
            }))
        }
        Err(FlowRunnerError::Transport(msg)) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("mesh transport: {msg}"),
                flow_id: None,
                flow_log: None,
            }),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
                flow_id: None,
                flow_log: None,
            }),
        )),
    }
}

fn validate_input(session_id: &str, message: &str) -> Result<(), String> {
    // Empty fields reject; SOL would parse the literal as empty and the
    // memory node would error noisily downstream — bail early.
    if session_id.trim().is_empty() {
        return Err("session_id required".into());
    }
    if message.trim().is_empty() {
        return Err("message required".into());
    }
    // SOL string literals are `"..."` with no escape sequences. A `"` in the
    // input would terminate the literal; a `|` collides with SIMP-016 wire
    // delimiters. Reject both. Newlines also corrupt the SOL source.
    for (field_name, field) in [("session_id", session_id), ("message", message)] {
        for ch in field.chars() {
            match ch {
                '"' => {
                    return Err(format!(
                        "{field_name}: '\"' forbidden (SOL has no string escapes)"
                    ));
                }
                '|' => {
                    return Err(format!(
                        "{field_name}: '|' forbidden (collides with wire delimiter)"
                    ));
                }
                '\r' | '\n' => {
                    return Err(format!("{field_name}: newline forbidden"));
                }
                _ => {}
            }
        }
    }
    if session_id.len() > 256 || message.len() > 4096 {
        return Err("input too long".into());
    }
    Ok(())
}

// ──────────────────────────── Main ─────────────────────────────────────────

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
    let state = AppState::try_new(cfg.clone())?;
    let addr: SocketAddr = state
        .cfg
        .bridge
        .listen_addr
        .parse()
        .map_err(|e| format!("listen_addr: {e}"))?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/chat", post(chat))
        .with_state(state);

    tracing::info!(
        listen = %addr,
        flow_template = %cfg.flow.template_path.display(),
        peers = %cfg.transport.peers_path.display(),
        "web bridge starting"
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Load the bridge's local libp2p secret from disk, or generate a new one if
/// the file does not exist. Mirrors `controller_runtime::load_or_generate_key`
/// so operators do not have to remember a manual `relix-cli` step before
/// first start. The file is gitignored (`dev-keys/*.key`).
fn load_or_generate_client_key(path: &std::path::Path) -> Result<[u8; 32], BridgeError> {
    if path.exists() {
        let bytes = std::fs::read(path)
            .map_err(|e| BridgeError::Config(format!("read client key {}: {e}", path.display())))?;
        if bytes.len() != 32 {
            return Err(BridgeError::Config(format!(
                "{}: expected 32-byte secret key, got {}",
                path.display(),
                bytes.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    } else {
        use rand::RngCore;
        let mut out = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut out);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| BridgeError::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::write(path, out).map_err(|e| {
            BridgeError::Config(format!("write client key {}: {e}", path.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mut p) = std::fs::metadata(path).map(|m| m.permissions()) {
                p.set_mode(0o600);
                let _ = std::fs::set_permissions(path, p);
            }
        }
        tracing::info!(path = %path.display(), "generated new bridge client key");
        Ok(out)
    }
}

/// Bridge-layer errors. Used at startup (config / identity bundle load).
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// Config / file-load failure.
    #[error("config: {0}")]
    Config(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_input_rejects_empty() {
        assert!(validate_input("", "x").is_err());
        assert!(validate_input("s", "").is_err());
        assert!(validate_input("   ", "x").is_err());
    }

    #[test]
    fn validate_input_rejects_quotes_pipes_and_newlines() {
        assert!(validate_input(r#"s"x"#, "msg").is_err());
        assert!(validate_input("s|x", "msg").is_err());
        assert!(validate_input("s\nx", "msg").is_err());
        assert!(validate_input("session", r#"msg"with"quote"#).is_err());
        assert!(validate_input("session", "msg|delim").is_err());
        assert!(validate_input("session", "msg\nline").is_err());
    }

    #[test]
    fn validate_input_rejects_too_long() {
        let long = "a".repeat(257);
        assert!(validate_input(&long, "x").is_err());
        let long_msg = "b".repeat(4097);
        assert!(validate_input("s", &long_msg).is_err());
    }

    #[test]
    fn validate_input_accepts_normal_text() {
        assert!(validate_input("demo-session", "hello world").is_ok());
        assert!(validate_input("s_1", "punctuation? yes!").is_ok());
    }

    #[test]
    fn bridge_config_parses() {
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
        assert_eq!(
            cfg.flow.template_path.to_string_lossy(),
            "flows/chat_template.sol"
        );
    }

    #[test]
    fn template_substitution_replaces_both_placeholders() {
        let tpl = r#"let s: str = "{{SESSION}}"; let m: str = "{{MESSAGE}}";"#;
        let rendered = tpl
            .replace("{{SESSION}}", "demo")
            .replace("{{MESSAGE}}", "hi");
        assert_eq!(rendered, r#"let s: str = "demo"; let m: str = "hi";"#);
    }
}
