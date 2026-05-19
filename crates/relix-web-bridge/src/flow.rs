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
use crate::validate::{validate_input, validate_url};
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
        capability_cache: Some(state.manifest_cache.clone()),
        mesh_client: state.mesh_client.clone(),
    };

    finalize_flow_run(FlowRunner::new(opts).run().await)
}

/// Translate a `FlowRunner::run` outcome into a `FlowOutcome` while making
/// VM-level halts (e.g. tool node returned `policy_denied`) visible as a
/// real error response instead of a 200 OK with an empty body. The bridge
/// stays a pure dispatcher; this only converts existing signals.
fn finalize_flow_run(
    res: Result<relix_runtime::flow_runner::FlowRunResult, FlowRunnerError>,
) -> Result<FlowOutcome, FlowExecError> {
    match res {
        Ok(result) => {
            // VM halted because a remote_call failed — surface the responder's
            // error envelope so curl / Open WebUI see a proper non-2xx rather
            // than an empty `reply: ""`. The flow log on disk still records
            // every step (RemoteCallIssued / RemoteCallFailed / FlowFailed).
            if let Some(err) = result.last_error {
                return Err(FlowExecError::Transport(format!(
                    "flow halted: {err} (flow_id={} flow_log={})",
                    result.flow_id,
                    result.flow_log_path.display()
                )));
            }
            Ok(FlowOutcome {
                reply: result.final_string.unwrap_or_default(),
                flow_id: result.flow_id.to_string(),
                trace_id: result.trace_id.to_string(),
                flow_log_path: result.flow_log_path.to_string_lossy().to_string(),
            })
        }
        Err(FlowRunnerError::Transport(m)) => Err(FlowExecError::Transport(m)),
        Err(e) => Err(FlowExecError::Internal(e.to_string())),
    }
}

/// Execute one chat turn through the configured *tool-augmented* SOL flow
/// template (M9). Returns the same [`FlowOutcome`] shape so callers don't
/// have to switch on the variant — the only difference at this layer is the
/// `{{TOOL_URL}}` substitution and the fact that the flow performs an extra
/// `tool.web_fetch` remote call before the AI step. SOL still owns the
/// orchestration; this function only selects the template.
pub async fn execute_chat_with_tool_flow(
    state: &AppState,
    session_id: &str,
    message: &str,
    url: &str,
) -> Result<FlowOutcome, FlowExecError> {
    let Some(tool_template) = state.tool_template.as_ref() else {
        return Err(FlowExecError::InvalidInput(
            "tool flow not configured (set [flow] tool_template_path in bridge config)".into(),
        ));
    };
    validate_input(session_id, message).map_err(FlowExecError::InvalidInput)?;
    validate_url(url).map_err(FlowExecError::InvalidInput)?;

    let rendered = tool_template
        .replace("{{SESSION}}", session_id)
        .replace("{{MESSAGE}}", message)
        .replace("{{TOOL_URL}}", url);

    let tmp = tempfile::Builder::new()
        .prefix("relix-bridge-chat-tool-")
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
        capability_cache: Some(state.manifest_cache.clone()),
        mesh_client: state.mesh_client.clone(),
    };

    finalize_flow_run(FlowRunner::new(opts).run().await)
}
