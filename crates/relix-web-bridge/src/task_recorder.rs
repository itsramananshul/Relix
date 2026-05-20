//! Bridge-side adapter that persists chat flows as Tasks on the
//! Coordinator peer.
//!
//! **Fail-soft contract.** Every method here returns silently on
//! Coordinator failure — log a `warn!`, do not propagate. A degraded
//! Coordinator must never block, fail, or crash a user's chat request.
//! The Coordinator is purely additive: when it's up, requests get
//! durable records; when it's down, requests still go through and
//! `task_id` ends up `None` in the response.
//!
//! All `task.*` calls go through `MeshClient::call(alias, envelope)` so
//! they benefit from the M11 connection pool *and* the A.4 reconnect
//! retry. The Coordinator's own admission pipeline (identity → policy →
//! handler → audit) runs on every call.

use std::sync::Arc;

use relix_core::bundle::Bundle;
use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::manifest::MeshClient;
use relix_runtime::nodes::coordinator::FailureClass;
use relix_runtime::transport::envelope::ResponseResult;

/// Owns the bridge-side fail-soft path for `task.*` calls.
///
/// Cheap to clone — internally it's an `Arc<MeshClient>` plus a small
/// metadata block.
#[derive(Clone)]
pub struct TaskRecorder {
    mesh: Arc<MeshClient>,
    alias: String,
    identity: Bundle,
    deadline_secs: i64,
}

impl TaskRecorder {
    pub fn new(mesh: Arc<MeshClient>, alias: String, identity: Bundle, deadline_secs: i64) -> Self {
        Self {
            mesh,
            alias,
            identity,
            deadline_secs,
        }
    }

    /// Create a Task. Returns `Some(task_id)` on success, `None` on any
    /// coordinator failure (logged at WARN). Callers MUST tolerate `None`
    /// and skip every subsequent event/update call for that request.
    pub async fn create(
        &self,
        title: &str,
        flow_template: &str,
        params_json: &str,
    ) -> Option<String> {
        // SIMP-016 pipe-delim. owner_subject_id left empty so the
        // Coordinator defaults to the caller's verified subject_id.
        let arg = format!("{title}|{flow_template}|{params_json}|");
        match self.call("task.create", arg.as_bytes()).await {
            Ok(body) => match std::str::from_utf8(&body) {
                Ok(s) => {
                    let id = s.trim().to_string();
                    if id.is_empty() {
                        tracing::warn!("coordinator returned empty task_id");
                        None
                    } else {
                        Some(id)
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "coordinator task.create response not utf-8");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "coordinator task.create failed; request persistence skipped");
                None
            }
        }
    }

    /// Append one event. Best-effort — failures log at WARN and are
    /// swallowed. Callers do not block on or retry this.
    pub async fn event(&self, task_id: &str, event_type: &str, payload: &str) {
        let arg = format!("{task_id}|{event_type}|{payload}");
        if let Err(e) = self.call("task.event", arg.as_bytes()).await {
            tracing::warn!(task_id, event_type, error = %e, "coordinator task.event failed");
        }
    }

    /// Terminal success update: status=completed + result + flow pointer.
    pub async fn complete(&self, task_id: &str, result: &str, flow_id: &str, flow_log_path: &str) {
        // task_id|status|result|flow_id|flow_log_path|error_kind|error_cause|failure_class
        let arg = format!("{task_id}|completed|{result}|{flow_id}|{flow_log_path}|||");
        if let Err(e) = self.call("task.update", arg.as_bytes()).await {
            tracing::warn!(task_id, error = %e, "coordinator task.update (complete) failed");
        }
    }

    /// Terminal failure update: status=failed + error_kind + error_cause +
    /// classified `FailureClass`. The class is what operators key off
    /// when deciding whether a retry is worth it (see
    /// `docs/retry-model.md`); the Coordinator stores it verbatim in
    /// `last_failure_class`.
    pub async fn fail(
        &self,
        task_id: &str,
        error_kind: u32,
        error_cause: &str,
        class: FailureClass,
    ) {
        // status + error_kind + error_cause + failure_class; no result/flow pointer.
        let class_str = class.as_str();
        let arg = format!("{task_id}|failed|||||{error_kind}|{error_cause}|{class_str}");
        if let Err(e) = self.call("task.update", arg.as_bytes()).await {
            tracing::warn!(task_id, error = %e, "coordinator task.update (fail) failed");
        }
    }

    /// Low-level wrapper. Builds an envelope, sends via MeshClient,
    /// decodes the response, returns the body bytes or a string error.
    async fn call(&self, method: &str, arg: &[u8]) -> Result<Vec<u8>, String> {
        let envelope = build_request(
            method,
            arg.to_vec(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = self
            .mesh
            .call(&self.alias, envelope)
            .await
            .map_err(|e| e.to_string())?;
        let resp = decode_response(&resp_bytes).map_err(|e| format!("decode: {e}"))?;
        match resp.res {
            ResponseResult::Ok(body) => Ok(body.to_vec()),
            ResponseResult::Err(env) => Err(format!("kind={} cause={}", env.kind, env.cause)),
            ResponseResult::StreamHandle(_) => Err("unexpected stream response".into()),
        }
    }
}

/// Truncate a string at `max_chars` characters (not bytes), appending an
/// ellipsis if anything was trimmed. Used to derive a Task title from
/// the user's message without dragging the whole prompt in.
pub fn make_title(prefix: &str, message: &str, max_chars: usize) -> String {
    let clean = message
        .lines()
        .next()
        .unwrap_or("")
        .replace(['|', '\t', '\r'], " ");
    let body = if clean.chars().count() <= max_chars {
        clean
    } else {
        let truncated: String = clean.chars().take(max_chars - 1).collect();
        format!("{truncated}…")
    };
    if prefix.is_empty() {
        body
    } else {
        format!("{prefix}: {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_title_truncates_long_messages() {
        let msg = "x".repeat(500);
        let t = make_title("chat", &msg, 32);
        assert!(t.starts_with("chat: "));
        // 32 chars in body inc. ellipsis
        let body_chars = t["chat: ".len()..].chars().count();
        assert_eq!(body_chars, 32);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn make_title_handles_short_message() {
        let t = make_title("chat", "hi", 32);
        assert_eq!(t, "chat: hi");
    }

    #[test]
    fn make_title_first_line_only() {
        let t = make_title("", "line one\nline two", 50);
        assert_eq!(t, "line one");
    }

    #[test]
    fn make_title_strips_pipe_and_tab() {
        let t = make_title("", "a|b\tc", 50);
        assert_eq!(t, "a b c");
    }
}
