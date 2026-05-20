//! Shared flow-execution helper used by every chat handler.
//!
//! `execute_chat_flow` is the bridge's single seam to `FlowRunner`. It:
//!
//!   1. Validates the input characters (SIMP-018).
//!   2. **(B1)** Creates a Task on the Coordinator when one is wired,
//!      fail-soft. Adds the `flow_selected` event.
//!   3. Renders the SOL template with the supplied session + message.
//!   4. Materialises the rendered SOL to a per-request tempfile.
//!   5. Calls `FlowRunner::run` on the existing libp2p path.
//!   6. **(B1)** Writes the terminal `task.update` (completed/failed) +
//!      a `flow_completed` / `flow_failed` event. All best-effort.
//!   7. Surfaces a structured outcome (now including `task_id`) so
//!      JSON / SSE / OpenAI handlers all project the same underlying
//!      flow result.

use std::path::PathBuf;

use crate::AppState;
use crate::task_recorder::{TaskRecorder, make_title};
use crate::validate::{validate_input, validate_url};
use relix_core::types::TraceId;
use relix_runtime::flow_runner::{FlowRunOptions, FlowRunner, FlowRunnerError};
use relix_runtime::nodes::coordinator::FailureClass;

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
    /// Coordinator-side Task id when persistence was wired AND the
    /// `task.create` call succeeded. `None` when the coordinator is
    /// absent or the call failed (fail-soft).
    pub task_id: Option<String>,
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

    // B1.2: best-effort task creation. None when coordinator is absent
    // or the call failed (TaskRecorder logs the warning).
    let task_id = create_task_fail_soft(
        state.task_recorder.as_ref(),
        "chat",
        "flows/chat_template.sol",
        &chat_params_json(session_id, message),
    )
    .await;
    // C2b.1: mint the trace_id upfront so the Coordinator's attempt
    // row and the per-flow event log share the same correlation id.
    let trace_id = TraceId::new();
    let trace_hex = trace_id.to_string();
    if let (Some(rec), Some(tid)) = (state.task_recorder.as_ref(), task_id.as_ref()) {
        rec.event(tid, "flow.started", "flows/chat_template.sol")
            .await;
        // C2a.2: status -> running opens a new attempt row on the
        // Coordinator. Fail-soft; recorded as WARN on failure.
        rec.start_running(tid, &trace_hex).await;
    }

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
        trace_id: Some(trace_id),
    };

    finalize_flow_run(
        FlowRunner::new(opts).run().await,
        state.task_recorder.as_ref(),
        task_id,
    )
    .await
}

/// Translate a `FlowRunner::run` outcome into a `FlowOutcome` while
/// making VM-level halts (e.g. tool node returned `policy_denied`)
/// visible as a real error response instead of a 200 OK with an empty
/// body, AND while writing the terminal task event/update when a
/// Coordinator is wired.
async fn finalize_flow_run(
    res: Result<relix_runtime::flow_runner::FlowRunResult, FlowRunnerError>,
    recorder: Option<&TaskRecorder>,
    task_id: Option<String>,
) -> Result<FlowOutcome, FlowExecError> {
    match res {
        Ok(result) => {
            // VM halted because a remote_call failed — surface the
            // responder's error envelope so curl / Open WebUI see a
            // proper non-2xx rather than an empty `reply: ""`. The
            // flow log on disk still records every step
            // (RemoteCallIssued / RemoteCallFailed / FlowFailed).
            if let Some(err) = result.last_error {
                let flow_id = result.flow_id.to_string();
                let flow_log_path = result.flow_log_path.to_string_lossy().to_string();
                let cause_for_event = err.clone();
                let kind = result.last_error_kind.unwrap_or(0);
                let class = FailureClass::from_kind(kind);
                if let (Some(rec), Some(tid)) = (recorder, task_id.as_ref()) {
                    rec.event(tid, "task.failed", &cause_for_event).await;
                    rec.fail(tid, kind, &cause_for_event, class).await;
                }
                return Err(FlowExecError::Transport(format!(
                    "flow halted: {err} (flow_id={flow_id} flow_log={flow_log_path})"
                )));
            }
            let reply = result.final_string.unwrap_or_default();
            let flow_id = result.flow_id.to_string();
            let trace_id = result.trace_id.to_string();
            let flow_log_path = result.flow_log_path.to_string_lossy().to_string();

            if let (Some(rec), Some(tid)) = (recorder, task_id.as_ref()) {
                // Keep the reply that goes into task_events short so the
                // ledger doesn't carry the full bodies (those live in the
                // per-flow event log on disk, which task.latest_flow_log_path
                // points at).
                let excerpt = truncate(&reply, 200);
                rec.event(tid, "task.completed", &excerpt).await;
                rec.complete(tid, &excerpt, &flow_id, &flow_log_path).await;
            }
            Ok(FlowOutcome {
                reply,
                flow_id,
                trace_id,
                flow_log_path,
                task_id,
            })
        }
        Err(FlowRunnerError::Transport(m)) => {
            if let (Some(rec), Some(tid)) = (recorder, task_id.as_ref()) {
                rec.event(tid, "task.failed", &m).await;
                // FlowRunner-layer transport failure (libp2p dial /
                // RPC), not a responder error envelope; classify as
                // transient and tag the kind as TRANSPORT so operator
                // tooling matches.
                rec.fail(
                    tid,
                    relix_core::types::error_kinds::TRANSPORT,
                    &m,
                    FailureClass::Transient,
                )
                .await;
            }
            Err(FlowExecError::Transport(m))
        }
        Err(e) => {
            let msg = e.to_string();
            if let (Some(rec), Some(tid)) = (recorder, task_id.as_ref()) {
                rec.event(tid, "task.failed", &msg).await;
                // Config / EventLog / Vm: not safe to retry without
                // operator action — surface as permanent so the CLI
                // colours it accordingly and bounded auto-retry (when
                // it lands) skips these.
                rec.fail(tid, 0, &msg, FailureClass::Permanent).await;
            }
            Err(FlowExecError::Internal(msg))
        }
    }
}

/// Execute one chat turn through the configured *tool-augmented* SOL flow
/// template (M9). Returns the same [`FlowOutcome`] shape so callers don't
/// have to switch on the variant — the only difference at this layer is the
/// `{{TOOL_URL}}` substitution, the fact that the flow performs an extra
/// `tool.web_fetch` remote call before the AI step, and the additional
/// `capability.invoked` event on the Task chronicle. SOL still owns
/// the orchestration; this function only selects the template.
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

    let task_id = create_task_fail_soft(
        state.task_recorder.as_ref(),
        "chat_with_tool",
        "flows/chat_with_tool.sol",
        &chat_with_tool_params_json(session_id, message, url),
    )
    .await;
    let trace_id = TraceId::new();
    let trace_hex = trace_id.to_string();
    if let (Some(rec), Some(tid)) = (state.task_recorder.as_ref(), task_id.as_ref()) {
        rec.event(tid, "flow.started", "flows/chat_with_tool.sol")
            .await;
        rec.start_running(tid, &trace_hex).await;
        // Pre-execution capability intent. Useful for operators
        // triaging failures: even if the tool peer rejects the URL,
        // the task chronicle says what was attempted. The payload
        // format `method=... target=...` is the convention for
        // capability.invoked when there's a meaningful target arg.
        rec.event(
            tid,
            "capability.invoked",
            &format!("method=tool.web_fetch target={url}"),
        )
        .await;
    }

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
        trace_id: Some(trace_id),
    };

    finalize_flow_run(
        FlowRunner::new(opts).run().await,
        state.task_recorder.as_ref(),
        task_id,
    )
    .await
}

/// Best-effort task creation. Returns `None` when persistence isn't
/// configured or the Coordinator call failed; the chat path continues
/// in either case (fail-soft per B1.9).
///
/// On success, emits a `task.created` chronology event so the
/// timeline is self-describing from line 1 (no need to cross-
/// reference `tasks.created_at`).
async fn create_task_fail_soft(
    recorder: Option<&TaskRecorder>,
    flow_label: &str,
    flow_template: &str,
    params_json: &str,
) -> Option<String> {
    let rec = recorder?;
    let title = make_title(flow_label, params_json, 64);
    let tid = rec.create(&title, flow_template, params_json).await?;
    rec.event(&tid, "task.created", flow_template).await;
    Some(tid)
}

/// Compact JSON for `task.create`'s `params_json`. Inline so we don't
/// pull serde_json's full machinery for two field types. The Coordinator
/// stores this verbatim and never parses it.
fn chat_params_json(session_id: &str, message: &str) -> String {
    let m = json_escape(message);
    let s = json_escape(session_id);
    format!(r#"{{"session_id":"{s}","message":"{m}"}}"#)
}

fn chat_with_tool_params_json(session_id: &str, message: &str, url: &str) -> String {
    let m = json_escape(message);
    let s = json_escape(session_id);
    let u = json_escape(url);
    format!(r#"{{"session_id":"{s}","message":"{m}","url":"{u}"}}"#)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Truncate a string at `n` characters (not bytes), appending an
/// ellipsis when trimmed. Used to keep task_events payloads compact.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_quotes_and_newlines() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("plain"), "plain");
    }

    #[test]
    fn truncate_at_char_boundary() {
        assert_eq!(truncate("abcdef", 10), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // multi-byte safety
        assert_eq!(truncate("αβγδε", 3), "αβ…");
    }

    #[test]
    fn chat_params_json_shape() {
        let s = chat_params_json("demo", "hello world");
        assert_eq!(s, r#"{"session_id":"demo","message":"hello world"}"#);
    }
}
