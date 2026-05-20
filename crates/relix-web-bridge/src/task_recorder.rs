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

    /// Mark a Task as `running` and (Coordinator-side) open a new
    /// attempt row. `trace_id` propagates to the attempt row so the
    /// per-flow event log and the attempt share a correlation id.
    /// Empty `trace_id` is accepted (Coordinator just stores NULL).
    pub async fn start_running(&self, task_id: &str, trace_id: &str) {
        // task_id|running||||||trace_id (9 slots).
        let arg = format!("{task_id}|running|||||||{trace_id}");
        if let Err(e) = self.call("task.update", arg.as_bytes()).await {
            tracing::warn!(task_id, error = %e, "coordinator task.update (start_running) failed");
        }
    }

    /// Terminal success update: status=completed + result + flow pointer.
    pub async fn complete(&self, task_id: &str, result: &str, flow_id: &str, flow_log_path: &str) {
        // task_id|status|result|flow_id|flow_log_path|error_kind|error_cause|failure_class|trace_id
        let arg = format!("{task_id}|completed|{result}|{flow_id}|{flow_log_path}||||");
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
        // status + error_kind + error_cause + failure_class; trace_id
        // slot left empty (no new attempt is opened on the fail path).
        let class_str = class.as_str();
        let arg = format!("{task_id}|failed|||||{error_kind}|{error_cause}|{class_str}|");
        if let Err(e) = self.call("task.update", arg.as_bytes()).await {
            tracing::warn!(task_id, error = %e, "coordinator task.update (fail) failed");
        }
    }

    /// Read-only `task.list` passthrough. Unlike the write methods
    /// this is NOT fail-soft — callers (e.g. the bridge's
    /// `/v1/tasks` endpoint) want to surface errors to the operator.
    ///
    /// Equivalent to `list_paginated(limit, 0, "")`. Kept for older
    /// call sites; new code should call `list_paginated` directly.
    #[allow(dead_code)]
    pub async fn list(&self, limit: usize) -> Result<String, String> {
        self.list_paginated(limit, 0, "").await
    }

    /// Server-side paginated + filtered passthrough. `status` empty
    /// means no filter.
    pub async fn list_paginated(
        &self,
        limit: usize,
        offset: usize,
        status: &str,
    ) -> Result<String, String> {
        let arg = format!("{limit}|{offset}|{status}");
        let bytes = self.call("task.list", arg.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.list utf8: {e}"))
    }

    /// `task.count` passthrough.
    pub async fn count(&self, status: &str) -> Result<String, String> {
        let bytes = self.call("task.count", status.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.count utf8: {e}"))
    }

    /// `task.export` passthrough. Returns the Coordinator's
    /// single-JSON archival artifact verbatim.
    pub async fn export(&self, task_id: &str) -> Result<String, String> {
        let bytes = self.call("task.export", task_id.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.export utf8: {e}"))
    }

    /// `task.compact_events` passthrough. Bridge supplies only
    /// the `dry-run` mode today — the destructive `delete` mode
    /// is gated by the chronicle-retention Step 3 design and
    /// would land here as a separate method once shipped.
    pub async fn compact_events_dry_run(&self, max_age_secs: i64) -> Result<String, String> {
        let arg = format!("{max_age_secs}|dry-run");
        let bytes = self.call("task.compact_events", arg.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.compact_events utf8: {e}"))
    }

    /// `task.list_cursor` passthrough. Wire format:
    /// `limit|status|cursor`. Empty status / cursor mean "no
    /// filter" / "first page."
    pub async fn list_cursor(
        &self,
        limit: usize,
        status: &str,
        cursor: &str,
    ) -> Result<String, String> {
        let arg = format!("{limit}|{status}|{cursor}");
        let bytes = self.call("task.list_cursor", arg.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.list_cursor utf8: {e}"))
    }

    /// `task.events` passthrough. Wire format:
    /// `task_id|after_id|limit|type|order`. `type` empty = no
    /// filter; `order` empty = asc. Kept for completeness; the
    /// bridge endpoint always goes through `events_filtered`.
    #[allow(dead_code)]
    pub async fn events(
        &self,
        task_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<String, String> {
        self.events_filtered(task_id, after_id, limit, "", "").await
    }

    /// `task.events` with type filter + order. Empty strings on
    /// `event_type` or `order` mean "no filter" / "default order".
    pub async fn events_filtered(
        &self,
        task_id: &str,
        after_id: i64,
        limit: usize,
        event_type: &str,
        order: &str,
    ) -> Result<String, String> {
        let arg = format!("{task_id}|{after_id}|{limit}|{event_type}|{order}");
        let bytes = self.call("task.events", arg.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.events utf8: {e}"))
    }

    /// Read-only `task.get` passthrough. Returns the Coordinator's
    /// `key=value` body verbatim; the caller parses.
    pub async fn get(&self, task_id: &str) -> Result<String, String> {
        let bytes = self.call("task.get", task_id.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.get utf8: {e}"))
    }

    /// Read-only `task.attempts` passthrough.
    pub async fn attempts(&self, task_id: &str) -> Result<String, String> {
        let bytes = self.call("task.attempts", task_id.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.attempts utf8: {e}"))
    }

    /// Operator-triggered `task.recover` passthrough. Not fail-soft;
    /// callers want the result of the scan.
    pub async fn recover(&self) -> Result<String, String> {
        let bytes = self.call("task.recover", b"").await?;
        String::from_utf8(bytes).map_err(|e| format!("task.recover utf8: {e}"))
    }

    /// Operator-triggered `task.retry` passthrough. Returns the
    /// Coordinator's body verbatim: `accepted attempt=N
    /// of_budget=M` or `exhausted retry_count=N budget=M`.
    /// The force-vs-not-retryable check is the bridge handler's
    /// responsibility (mirrors the CLI's same-pattern guard).
    pub async fn retry(&self, task_id: &str) -> Result<String, String> {
        let bytes = self.call("task.retry", task_id.as_bytes()).await?;
        String::from_utf8(bytes).map_err(|e| format!("task.retry utf8: {e}"))
    }

    /// Operator-triggered cancellation. Two steps:
    ///
    /// 1. Append a `task.cancelled` event with the operator-
    ///    supplied reason (chronicle visibility).
    /// 2. Update the task's status to `cancelled`.
    ///
    /// HONEST CAVEAT: the runtime has no flow-cancellation
    /// protocol today. A currently-executing flow continues
    /// to run; its eventual write-back may overwrite the
    /// `cancelled` status. The dashboard surfaces this
    /// caveat in the confirm dialog. Phase 2 work introduces
    /// real flow-side cancellation.
    pub async fn cancel(&self, task_id: &str, reason: &str) -> Result<(), String> {
        // Best-effort chronicle event first.
        let event_arg = format!(
            "{task_id}|task.cancelled|{}",
            if reason.is_empty() {
                "operator-cancelled".to_string()
            } else {
                reason.to_string()
            }
        );
        if let Err(e) = self.call("task.event", event_arg.as_bytes()).await {
            // Non-fatal: continue to the status update so the
            // operator sees the task move state even if the
            // chronicle event didn't land.
            tracing::warn!(task_id, error = %e, "coordinator task.event (cancel) failed");
        }
        // task_id|status|result|flow_id|flow_log_path|error_kind|error_cause|failure_class|trace_id
        let update_arg = format!("{task_id}|cancelled||||||||");
        self.call("task.update", update_arg.as_bytes())
            .await
            .map(|_| ())
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
