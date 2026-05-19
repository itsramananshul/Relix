//! Shared flow-execution helper used by every chat handler.
//!
//! `execute_chat_flow` is the bridge's single seam to `FlowRunner`. It:
//!
//!   1. Validates the input characters (SIMP-018).
//!   2. Renders the SOL template with the supplied session + message.
//!   3. Materialises the rendered SOL to a per-request tempfile.
//!   4. Calls `FlowRunner::run` on the existing libp2p path.
//!   5. Surfaces a structured outcome so JSON / SSE / OpenAI handlers all
//!      project the same underlying flow result.

use std::path::PathBuf;

use crate::AppState;
use crate::validate::validate_input;
use relix_runtime::flow_runner::{FlowRunOptions, FlowRunner, FlowRunnerError};

/// Successful end-to-end chat flow.
#[derive(Debug, Clone)]
pub struct FlowOutcome {
    /// The provider's reply text, resolved from the VM's final heap string.
    pub reply: String,
    /// 16-byte FlowId, hex-encoded.
    pub flow_id: String,
    /// 16-byte TraceId, hex-encoded.
    pub trace_id: String,
    /// On-disk path of the per-flow event log.
    pub flow_log_path: String,
}

/// Categorised failure so handlers can pick the right HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum FlowExecError {
    /// Invalid request body / characters — 400 Bad Request.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Mesh transport / dial / RPC layer failure — 502 Bad Gateway.
    #[error("mesh transport: {0}")]
    Transport(String),
    /// Anything else surfaced by the runner — 500 Internal Server Error.
    #[error("{0}")]
    Internal(String),
}

/// Execute one chat turn through the configured SOL flow template.
pub async fn execute_chat_flow(
    state: &AppState,
    session_id: &str,
    message: &str,
) -> Result<FlowOutcome, FlowExecError> {
    validate_input(session_id, message).map_err(FlowExecError::InvalidInput)?;

    let rendered = state
        .template
        .replace("{{SESSION}}", session_id)
        .replace("{{MESSAGE}}", message);

    let tmp = tempfile::Builder::new()
        .prefix("relix-bridge-chat-")
        .suffix(".sol")
        .tempfile()
        .map_err(|e| FlowExecError::Internal(format!("tempfile: {e}")))?;
    std::fs::write(tmp.path(), rendered.as_bytes())
        .map_err(|e| FlowExecError::Internal(format!("write tempfile: {e}")))?;
    let flow_path: PathBuf = tmp.path().to_path_buf();

    let opts = FlowRunOptions {
        flow_path,
        identity_bundle: state.identity_bundle.clone(),
        client_key: state.client_key,
        peers: state.peers.clone(),
        data_dir: state.cfg.transport.data_dir.clone(),
        deadline_secs: state.cfg.transport.deadline_secs,
    };

    match FlowRunner::new(opts).run().await {
        Ok(result) => Ok(FlowOutcome {
            reply: result.final_string.unwrap_or_default(),
            flow_id: result.flow_id.to_string(),
            trace_id: result.trace_id.to_string(),
            flow_log_path: result.flow_log_path.to_string_lossy().to_string(),
        }),
        Err(FlowRunnerError::Transport(m)) => Err(FlowExecError::Transport(m)),
        Err(e) => Err(FlowExecError::Internal(e.to_string())),
    }
}
