//! Read-only task inspection endpoints (`/v1/tasks` family).
//!
//! Translation-only by design: every endpoint forwards to a
//! Coordinator capability through the existing `TaskRecorder` and
//! reshapes the response as JSON. The bridge does NOT add
//! orchestration logic, filtering policy, or scheduling — it only
//! translates between HTTP/JSON and the Coordinator's pipe-delimited
//! wire format.
//!
//! Endpoints:
//!
//! - `GET /v1/tasks` — list recent tasks. Optional `?status=` filter
//!   is applied client-side (Coordinator's `task.list` doesn't filter
//!   today). Optional `?limit=` (default 50, capped by Coordinator).
//! - `GET /v1/tasks/:id` — return one task's header + chronicle.
//! - `GET /v1/tasks/:id/attempts` — return that task's attempt rows.
//!
//! All three return `503 Service Unavailable` when the bridge has no
//! Coordinator wired, and `502 Bad Gateway` when the Coordinator call
//! fails (transient mesh error, policy denial, unknown task on the
//! `get`/`attempts` paths — the responder's cause string is
//! propagated in the JSON body for triage).
//!
//! Authentication: there is none at the HTTP layer. The bridge's
//! identity already gates the underlying `task.*` capabilities on
//! the Coordinator's admission pipeline. If you expose these
//! endpoints publicly, put a reverse proxy in front; the model is
//! "bridge identity == operator surface".

use std::collections::BTreeMap;

use std::time::Duration;

use async_stream::stream;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::activity::{TaskControlActivity, append_task_control_activity};
use crate::config::AppState;
use crate::intervention_audit::new_correlation_id;
use crate::tenant::{CURRENT_TENANT, DEFAULT_TENANT, current_subject, current_tenant};

/// Compact task line returned by `GET /v1/tasks`.
#[derive(Debug, Serialize)]
pub struct TaskListEntry {
    pub task_id: String,
    pub status: String,
    pub title: String,
    /// Unix seconds when the task row was last updated in the
    /// Coordinator ledger. Surfaced so the dashboard can render
    /// "running for X" / "stale for X" age labels on the
    /// per-task row. Optional because old payload shapes (and
    /// truncated rows) may not carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    /// M63: operator-set investigation marker timestamp.
    /// `None` when unset (or when an older Coordinator emits
    /// a 4-column row). Dashboard renders an "under
    /// investigation" badge when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub investigation_marked_at: Option<i64>,
}

/// Detailed task body returned by `GET /v1/tasks/:id`.
#[derive(Debug, Serialize)]
pub struct TaskDetail {
    pub task_id: String,
    /// All header `key=value` fields from `task.get`, plus the
    /// derived `event_count`. Kept as a string map for forward
    /// compatibility with new C2/C3 fields the Coordinator may add.
    pub header: BTreeMap<String, String>,
    pub events: Vec<TaskEvent>,
}

#[derive(Debug, Serialize)]
pub struct TaskEvent {
    pub event_id: i64,
    pub ts: i64,
    pub event_type: String,
    pub payload: String,
    /// S2 typed envelope fields. All optional so v0 events render
    /// identically to before. `schema_version` defaults to 0 when
    /// the Coordinator omits it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub schema_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Embedded JSON document (already-encoded). The bridge
    /// surfaces it verbatim so dashboards can re-parse without
    /// double-decoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<serde_json::Value>,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// One attempt row returned by `GET /v1/tasks/:id/attempts`.
#[derive(Debug, Serialize)]
pub struct TaskAttempt {
    pub attempt_num: i64,
    pub status: String,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
}

/// One-line operator-friendly summary returned by
/// `GET /v1/tasks/:id/summary`. Same shape as the CLI's
/// `task get --pretty` first line, but JSON-typed so dashboards can
/// project columns directly. All fields are Optional so the response
/// is honest about what's known versus inferred.
#[derive(Debug, Serialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_count: Option<i64>,
    /// Wall-clock seconds between `started_at` and `updated_at` for
    /// terminal states (completed / failed / cancelled / interrupted).
    /// `None` for in-flight states.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<i64>,
    /// `started_at` of the task, present for running and terminal
    /// states. `None` when the task is still pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_reason: Option<String>,
    /// `<retry_count>/<max_retries>` text under bounded; `None`
    /// when retry_policy is `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Skip the first N rows of the (filtered) ordering. Server-side
    /// since Priority A; the Coordinator hits its `tasks_status`
    /// index when `status` is set.
    #[serde(default)]
    pub offset: Option<usize>,
}

/// `GET /v1/tasks` — list tasks. Server-side paginated and filtered
/// via the Coordinator's `task.list` (since Priority A). The
/// previous client-side status-filter behaviour is unchanged for
/// callers: filtering still works, it just no longer requires
/// over-fetching.
pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<TaskListEntry>>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let status = q.status.as_deref().unwrap_or("");
    let body = rec
        .list_paginated(limit, offset, status)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(ApiError { error: e })))?;
    let mut out = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() != 3 {
            continue;
        }
        out.push(TaskListEntry {
            task_id: parts[0].to_string(),
            status: parts[1].to_string(),
            title: parts[2].to_string(),
            // The older list_paginated payload omits updated_at
            // and investigation_marked_at; the dashboard renders
            // "—" when None, so this is honest rather than
            // fabricated.
            updated_at: None,
            investigation_marked_at: None,
        });
    }
    Ok(Json(out))
}

#[derive(Debug, Serialize)]
pub struct TaskCursorPage {
    pub items: Vec<TaskListEntry>,
    /// Opaque continuation token. Pass back as `?cursor=...` on
    /// the next request. `None` after the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CursorQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub status: Option<String>,
    /// Opaque continuation token from the previous response's
    /// `next_cursor`. Empty / absent = first page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /v1/tasks/cursor?limit=N&status=...&cursor=...` —
/// cursor-paginated list. Stable under concurrent inserts and
/// updates (unlike `/v1/tasks?offset=N` which can repeat or skip
/// rows when ordering ties shift). Use this when paginating a
/// live ledger.
///
/// Response shape `{items: [...], next_cursor: "..."}`. The cursor
/// is opaque to the caller; pass back what we returned.
pub async fn list_cursor(
    State(state): State<AppState>,
    Query(q): Query<CursorQuery>,
) -> Result<Json<TaskCursorPage>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let limit = q.limit.unwrap_or(50);
    let status = q.status.as_deref().unwrap_or("");
    let cursor = q.cursor.as_deref().unwrap_or("");
    let body = rec
        .list_cursor(limit, status, cursor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(ApiError { error: e })))?;
    let (items, next_cursor) = parse_cursor_body(&body);
    Ok(Json(TaskCursorPage { items, next_cursor }))
}

/// `GET /v1/tasks/count` — total count, optionally filtered by
/// status. Returns `{ "count": N }`. Drives pagination UIs that
/// want "N of M" without walking every page.
pub async fn count(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<CountResponse>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let status = q.status.as_deref().unwrap_or("");
    let body = rec
        .count(status)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(ApiError { error: e })))?;
    let n = parse_count_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!("coordinator task.count returned unexpected body: {body}"),
        }),
    ))?;
    Ok(Json(CountResponse { count: n }))
}

#[derive(Debug, Serialize)]
pub struct CountResponse {
    pub count: i64,
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskDetail>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = rec
        .get(&id)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_task_body(&id, &body)))
}

pub async fn summary(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskSummary>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = rec
        .get(&id)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    let summary = derive_summary(&id, &body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: "coordinator returned a task body without status".into(),
        }),
    ))?;
    Ok(Json(summary))
}

#[derive(Debug, Serialize)]
pub struct RecoverResponse {
    pub recovered: Vec<String>,
    pub count: usize,
}

/// `POST /v1/tasks/recover` — operator-triggered recovery scan.
/// Promotes overdue `running` tasks to `interrupted` and closes
/// the open attempt with `failure_class=timeout`. Idempotent.
///
/// Same write-only-with-no-HTTP-auth caveat as the chat endpoints:
/// put a reverse proxy in front before exposing this beyond
/// loopback. The Coordinator's policy still applies (the bridge's
/// identity must be admitted to `task.recover`).
pub async fn recover(
    State(state): State<AppState>,
) -> Result<Json<RecoverResponse>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        state.intervention_audit.record_with_id(
            "anon",
            "recover",
            "all",
            "error",
            "no coordinator configured",
            corr,
        );
        record_task_control_activity(
            &state,
            "task.recover",
            None,
            "all",
            "error",
            "no coordinator configured",
        );
        return Err(no_coordinator());
    };
    let body = match rec.recover().await {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "recover",
                "all",
                "error",
                format!("coord call failed: {e}"),
                corr,
            );
            record_task_control_activity(
                &state,
                "task.recover",
                None,
                "all",
                "error",
                "coordinator call failed",
            );
            return Err((StatusCode::BAD_GATEWAY, Json(ApiError { error: e })));
        }
    };
    let (ids, _count) = parse_recover_body(&body);
    let count = ids.len();
    // First 5 IDs in the detail is enough to scan; the full
    // list lives in the response body.
    let preview: Vec<&str> = ids.iter().take(5).map(String::as_str).collect();
    let detail = if count == 0 {
        "no overdue tasks".to_string()
    } else {
        format!(
            "recovered {count} task(s): {}{}",
            preview.join(", "),
            if count > preview.len() { ", …" } else { "" }
        )
    };
    state
        .intervention_audit
        .record_with_id("anon", "recover", "all", "ok", detail, corr);
    record_task_control_activity(
        &state,
        "task.recover",
        None,
        "all",
        "ok",
        format!("recovered_count={count}"),
    );
    Ok(Json(RecoverResponse {
        recovered: ids,
        count,
    }))
}

#[derive(Debug, Deserialize, Default)]
pub struct RetryQuery {
    /// Override the bridge-side guard that refuses retries on
    /// non-retryable failure classes (policy_denied /
    /// invalid_args / permanent). Mirrors the CLI's `--force`.
    #[serde(default)]
    pub force: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct RetryResponse {
    /// One of `accepted` / `exhausted` / `refused`.
    pub outcome: String,
    /// Raw Coordinator body line (or refusal explanation).
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub of_budget: Option<i64>,
    /// When refused, the failure class that triggered the
    /// guard. Operators see this to decide whether to retry
    /// with force=true after inspection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_class: Option<String>,
}

/// Non-retryable failure classes per docs/retry-model.md. Same
/// list the CLI's `retry_blocked_by_class` enforces.
const NON_RETRYABLE_CLASSES: &[&str] = &["policy_denied", "invalid_args", "permanent"];

/// Task statuses that allow cancel. Terminal states reject so
/// operators don't mark completed tasks as cancelled and lose
/// the original outcome.
const CANCELLABLE_STATUSES: &[&str] = &[
    "pending",
    "running",
    "retrying",
    "interrupted",
    "awaiting_input",
];

#[derive(Debug, Deserialize, Default)]
pub struct CancelReq {
    /// Operator-supplied reason. Surfaced in the
    /// `task.cancelled` chronicle event payload. Empty body
    /// → defaults to "operator-cancelled".
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CancelResp {
    pub task_id: String,
    pub prior_status: String,
    pub new_status: String,
    /// Honest note: the runtime has no flow-cancellation today.
    /// A running flow's write-back may overwrite the cancelled
    /// status. Surfaced so dashboards can warn operators.
    pub flow_still_running: bool,
}

/// `POST /v1/tasks/:id/cancel` — mark a task as cancelled in
/// the Coordinator ledger. Refuses terminal states (completed
/// / failed / cancelled).
///
/// HONEST: cancellation is metadata-only today. The runtime
/// has no flow-side cancellation protocol; an in-flight flow
/// continues and may overwrite the status when it finishes.
/// `flow_still_running: true` is returned when prior_status
/// was `running` or `retrying` so the dashboard can warn
/// the operator explicitly.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CancelReq>,
) -> Result<Json<CancelResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    // Pre-flight: read current status so we can reject terminal
    // tasks + report the prior status in the response.
    let body = rec
        .get(&id)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    let prior_status = body
        .lines()
        .find_map(|line| line.strip_prefix("status="))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    if !CANCELLABLE_STATUSES.contains(&prior_status.as_str()) {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiError {
                error: format!(
                    "task is in terminal status '{prior_status}'; cancel rejected (allowed: {})",
                    CANCELLABLE_STATUSES.join(", ")
                ),
            }),
        ));
    }
    let reason = req.reason.unwrap_or_default();
    if let Err(e) = rec.cancel(&id, &reason).await {
        state.intervention_audit.record_with_id(
            "anon",
            "cancel",
            &id,
            "error",
            format!("coord cancel failed: {e}"),
            corr,
        );
        record_task_control_activity(
            &state,
            "task.cancel",
            Some(&id),
            &id,
            "error",
            "coordinator task.cancel failed",
        );
        return Err((gateway_status_for(&e), Json(ApiError { error: e })));
    }
    let flow_still_running = matches!(prior_status.as_str(), "running" | "retrying");
    let detail = format!(
        "{prior_status}→cancelled{}{}",
        if reason.is_empty() {
            String::new()
        } else {
            format!(" · reason={reason}")
        },
        if flow_still_running {
            " · flow still running"
        } else {
            ""
        }
    );
    state
        .intervention_audit
        .record_with_id("anon", "cancel", &id, "ok", detail, corr);
    record_task_control_activity(
        &state,
        "task.cancel",
        Some(&id),
        &id,
        "ok",
        format!(
            "prior_status={prior_status}; new_status=cancelled; reason_len={}; flow_still_running={flow_still_running}",
            reason.len()
        ),
    );
    Ok(Json(CancelResp {
        task_id: id,
        prior_status,
        new_status: "cancelled".to_string(),
        flow_still_running,
    }))
}

/// `POST /v1/tasks/:id/retry?force=<bool>` — operator-triggered
/// retry. Returns a typed envelope distinguishing the three
/// outcomes (accepted / exhausted / refused) so dashboards can
/// react accordingly. The `force` query param defaults to false;
/// when false the bridge refuses retries on non-retryable
/// failure classes (mirrors the CLI's `--force`).
pub async fn retry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<RetryQuery>,
) -> Result<Json<RetryResponse>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let force = q.force.unwrap_or(false);

    // Guard: refuse non-retryable failure classes unless force.
    // Fetch the task get to read last_failure_class. Skip the
    // guard if the get itself fails — the Coordinator will
    // surface the appropriate error.
    if !force && let Ok(body) = rec.get(&id).await {
        let class = parse_failure_class_from_body(&body);
        if let Some(c) = class.as_deref()
            && NON_RETRYABLE_CLASSES.contains(&c)
        {
            state.intervention_audit.record_with_id(
                "anon",
                "retry",
                &id,
                "refused",
                format!("non-retryable failure_class={c}"),
                corr,
            );
            record_task_control_activity(
                &state,
                "task.retry",
                Some(&id),
                &id,
                "refused",
                format!("failure_class={c}; force=false"),
            );
            return Ok(Json(RetryResponse {
                outcome: "refused".to_string(),
                detail: format!(
                    "last_failure_class={c} is non-retryable. Inspect the flow + chronicle, then pass force=true to override."
                ),
                attempt: None,
                of_budget: None,
                last_failure_class: Some(c.to_string()),
            }));
        }
    }

    let body = match rec.retry(&id).await {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "retry",
                &id,
                "error",
                format!("coord retry failed: {e}"),
                corr,
            );
            record_task_control_activity(
                &state,
                "task.retry",
                Some(&id),
                &id,
                "error",
                format!("coordinator task.retry failed; force={force}"),
            );
            return Err((gateway_status_for(&e), Json(ApiError { error: e })));
        }
    };
    let parsed = parse_retry_body(&body);
    let force_suffix = if force { " · force=true" } else { "" };
    let outcome_label = match parsed.outcome.as_str() {
        "accepted" => "ok",
        "exhausted" | "refused" => "refused",
        _ => "ok",
    };
    state.intervention_audit.record_with_id(
        "anon",
        "retry",
        &id,
        outcome_label,
        format!("{}{}", parsed.detail, force_suffix),
        corr,
    );
    record_task_control_activity(
        &state,
        "task.retry",
        Some(&id),
        &id,
        outcome_label,
        format!(
            "outcome={}; force={force}; attempt={}; of_budget={}",
            parsed.outcome,
            parsed
                .attempt
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            parsed
                .of_budget
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        ),
    );
    Ok(Json(parsed))
}

// ── M60: operator notes ────────────────────────────────────

/// Hard cap on bridge-side note body length. Tighter than the
/// coordinator's `MAX_OPERATOR_NOTE_LEN` so a misbehaving
/// dashboard client doesn't waste a coord round-trip just to
/// be rejected. Mirror the coord constant if it shrinks.
const NOTE_BRIDGE_MAX_LEN: usize = 2_000;

#[derive(Debug, Deserialize)]
pub struct NoteReq {
    /// The annotation text. Non-empty after trimming; capped
    /// at NOTE_BRIDGE_MAX_LEN bytes. Control chars (NUL, etc.)
    /// rejected so a pathological dashboard input can't break
    /// downstream chronicle renderers.
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct NoteResp {
    pub task_id: String,
    /// Coordinator-assigned event_id so the dashboard can
    /// scroll the chronicle to the new note immediately.
    pub event_id: i64,
}

/// W2-001c: response shape for `POST /v1/tasks/:id/replay`.
#[derive(Debug, Serialize)]
pub struct ReplayResponse {
    /// The original task that was replayed.
    pub original_task_id: String,
    /// The freshly-minted replay task. 32 hex chars.
    pub new_task_id: String,
}

/// W2-001c: `POST /v1/tasks/:id/replay` — operator-triggered
/// replay. Asks the Coordinator to clone the original task
/// (preserves flow_template / params / retry-policy /
/// origin_surface; fresh retry_count) and wire a `retried_from`
/// edge from the new task back to the original. Returns the
/// new task_id so the dashboard can navigate to it.
pub async fn replay(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ReplayResponse>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = match rec.replay(&id).await {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "replay",
                &id,
                "error",
                format!("coord replay failed: {e}"),
                corr,
            );
            record_task_control_activity(
                &state,
                "task.replay",
                Some(&id),
                &id,
                "error",
                "coordinator task.replay failed",
            );
            return Err((gateway_status_for(&e), Json(ApiError { error: e })));
        }
    };
    let new_task_id = body.trim().to_string();
    if new_task_id.is_empty() {
        record_task_control_activity(
            &state,
            "task.replay",
            Some(&id),
            &id,
            "error",
            "coordinator returned empty replay task id",
        );
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "coordinator replay returned empty body".into(),
            }),
        ));
    }
    state.intervention_audit.record_with_id(
        "anon",
        "replay",
        &id,
        "ok",
        format!("new_task_id={new_task_id}"),
        corr,
    );
    record_task_control_activity(
        &state,
        "task.replay",
        Some(&id),
        &id,
        "ok",
        format!("new_task_id={new_task_id}"),
    );
    Ok(Json(ReplayResponse {
        original_task_id: id,
        new_task_id,
    }))
}

/// `POST /v1/tasks/:id/note` — append an operator annotation
/// to a task's chronicle. Validates the note body bridge-side
/// (length, control chars), forwards to the Coordinator's
/// `task.note` capability, and records the action into the
/// intervention audit ring.
pub async fn note(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<NoteReq>,
) -> Result<Json<NoteResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        state.intervention_audit.record_with_id(
            "anon",
            "task_note",
            &id,
            "error",
            "no coordinator",
            corr,
        );
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let trimmed = req.note.trim();
    if trimmed.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "note: required (non-empty after trim)".into(),
            }),
        ));
    }
    if trimmed.len() > NOTE_BRIDGE_MAX_LEN {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: format!(
                    "note: too long (max {NOTE_BRIDGE_MAX_LEN} bytes, got {})",
                    trimmed.len()
                ),
            }),
        ));
    }
    // Reject control chars (except common whitespace) so a
    // pathological client can't ship NULs into the chronicle.
    if trimmed
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "note: must not contain control characters".into(),
            }),
        ));
    }
    let body = match rec.note(&id, trimmed).await {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "task_note",
                &id,
                "error",
                format!("coord task.note failed: {e}"),
                corr,
            );
            return Err((gateway_status_for(&e), Json(ApiError { error: e })));
        }
    };
    let event_id: i64 = body
        .lines()
        .find_map(|l| l.strip_prefix("event_id="))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    // Short preview in the audit detail — full text lives in
    // the chronicle event itself.
    let preview = if trimmed.len() <= 80 {
        trimmed.to_string()
    } else {
        let mut end = 80;
        while !trimmed.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    };
    state.intervention_audit.record_with_id(
        "anon",
        "task_note",
        &id,
        "ok",
        format!("event_id={event_id} · {preview}"),
        corr,
    );
    Ok(Json(NoteResp {
        task_id: id,
        event_id,
    }))
}

// ── M71: freeze / unfreeze ─────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct FreezeReq {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FreezeResp {
    pub task_id: String,
    pub prior_status: String,
    pub new_status: String,
    /// HONEST: like pause, no runtime gate primitive yet.
    /// True when prior_status was a live execution state.
    pub flow_still_running: bool,
}

#[derive(Debug, Serialize)]
pub struct UnfreezeResp {
    pub task_id: String,
    pub pre_freeze_status: String,
    pub new_status: String,
}

/// `POST /v1/tasks/:id/freeze` — operator-initiated workflow
/// freeze (M71). Forwards to the Coordinator's `task.freeze`,
/// records into the intervention audit ring with a M68
/// correlation id.
pub async fn freeze(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<FreezeReq>,
) -> Result<Json<FreezeResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        state.intervention_audit.record_with_id(
            "anon",
            "freeze",
            &id,
            "error",
            "no coordinator",
            corr,
        );
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let trimmed_reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(r) = trimmed_reason {
        if r.len() > NOTE_BRIDGE_MAX_LEN {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: format!(
                        "reason: too long (max {NOTE_BRIDGE_MAX_LEN} bytes, got {})",
                        r.len()
                    ),
                }),
            ));
        }
        if r.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: "reason: must not contain control characters".into(),
                }),
            ));
        }
    }
    let body = match rec.freeze(&id, trimmed_reason).await {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "freeze",
                &id,
                "error",
                format!("coord task.freeze failed: {e}"),
                corr,
            );
            return Err((gateway_status_for(&e), Json(ApiError { error: e })));
        }
    };
    let prior_status = body
        .lines()
        .find_map(|l| l.strip_prefix("prior_status="))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let flow_still_running = matches!(prior_status.as_str(), "running" | "retrying");
    let detail = format!(
        "{prior_status}→frozen{}{}",
        trimmed_reason
            .map(|r| format!(" · {r}"))
            .unwrap_or_default(),
        if flow_still_running {
            " · flow still running"
        } else {
            ""
        }
    );
    state
        .intervention_audit
        .record_with_id("anon", "freeze", &id, "ok", detail, corr);
    Ok(Json(FreezeResp {
        task_id: id,
        prior_status,
        new_status: "frozen".to_string(),
        flow_still_running,
    }))
}

/// `POST /v1/tasks/:id/unfreeze` — operator-initiated
/// unfreeze (M71).
pub async fn unfreeze(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<UnfreezeResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        state.intervention_audit.record_with_id(
            "anon",
            "unfreeze",
            &id,
            "error",
            "no coordinator",
            corr,
        );
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = match rec.unfreeze(&id).await {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "unfreeze",
                &id,
                "error",
                format!("coord task.unfreeze failed: {e}"),
                corr,
            );
            return Err((gateway_status_for(&e), Json(ApiError { error: e })));
        }
    };
    let pre_freeze_status = body
        .lines()
        .find_map(|l| l.strip_prefix("pre_freeze_status="))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "frozen".to_string());
    state.intervention_audit.record_with_id(
        "anon",
        "unfreeze",
        &id,
        "ok",
        format!("frozen→pending (was {pre_freeze_status})"),
        corr,
    );
    Ok(Json(UnfreezeResp {
        task_id: id,
        pre_freeze_status,
        new_status: "pending".to_string(),
    }))
}

// ── M65: pause / resume ────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct PauseReq {
    /// Optional operator-supplied reason. Capped at
    /// `NOTE_BRIDGE_MAX_LEN`. Surfaced in the
    /// `task.paused` chronicle event.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PauseResp {
    pub task_id: String,
    pub prior_status: String,
    pub new_status: String,
    /// HONEST: the runtime has no flow-pause primitive today.
    /// When prior_status was `running`/`retrying`, an in-flight
    /// flow continues — the dashboard surfaces this caveat in
    /// the confirm UI. Same shape as cancel's flow_still_running.
    pub flow_still_running: bool,
}

#[derive(Debug, Serialize)]
pub struct ResumeResp {
    pub task_id: String,
    /// Status before the most recent pause, recovered from the
    /// chronicle. `paused` when no prior `task.paused` event
    /// can be found (defensive fallback).
    pub pre_pause_status: String,
    pub new_status: String,
}

/// `POST /v1/tasks/:id/pause` — operator-initiated pause.
/// Forwards to the Coordinator's `task.pause`, records into
/// the intervention audit ring.
pub async fn pause(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PauseReq>,
) -> Result<Json<PauseResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        state.intervention_audit.record_with_id(
            "anon",
            "pause",
            &id,
            "error",
            "no coordinator",
            corr,
        );
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let trimmed_reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(r) = trimmed_reason {
        if r.len() > NOTE_BRIDGE_MAX_LEN {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: format!(
                        "reason: too long (max {NOTE_BRIDGE_MAX_LEN} bytes, got {})",
                        r.len()
                    ),
                }),
            ));
        }
        if r.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: "reason: must not contain control characters".into(),
                }),
            ));
        }
    }
    let body = match rec.pause(&id, trimmed_reason).await {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "pause",
                &id,
                "error",
                format!("coord task.pause failed: {e}"),
                corr,
            );
            return Err((gateway_status_for(&e), Json(ApiError { error: e })));
        }
    };
    let prior_status = body
        .lines()
        .find_map(|l| l.strip_prefix("prior_status="))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let flow_still_running = matches!(prior_status.as_str(), "running" | "retrying");
    let detail = format!(
        "{prior_status}→paused{}{}",
        trimmed_reason
            .map(|r| format!(" · {r}"))
            .unwrap_or_default(),
        if flow_still_running {
            " · flow still running"
        } else {
            ""
        }
    );
    state
        .intervention_audit
        .record_with_id("anon", "pause", &id, "ok", detail, corr);
    Ok(Json(PauseResp {
        task_id: id,
        prior_status,
        new_status: "paused".to_string(),
        flow_still_running,
    }))
}

/// `POST /v1/tasks/:id/resume` — operator-initiated resume.
/// Forwards to the Coordinator's `task.resume`. Status must
/// be `paused`; the new status is always `pending`.
pub async fn resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ResumeResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        state.intervention_audit.record_with_id(
            "anon",
            "resume",
            &id,
            "error",
            "no coordinator",
            corr,
        );
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = match rec.resume(&id).await {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "resume",
                &id,
                "error",
                format!("coord task.resume failed: {e}"),
                corr,
            );
            return Err((gateway_status_for(&e), Json(ApiError { error: e })));
        }
    };
    let pre_pause_status = body
        .lines()
        .find_map(|l| l.strip_prefix("pre_pause_status="))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "paused".to_string());
    state.intervention_audit.record_with_id(
        "anon",
        "resume",
        &id,
        "ok",
        format!("paused→pending (was {pre_pause_status})"),
        corr,
    );
    Ok(Json(ResumeResp {
        task_id: id,
        pre_pause_status,
        new_status: "pending".to_string(),
    }))
}

// ── M62: investigation marker ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct InvestigationReq {
    /// `true` to mark, `false` to clear. Required.
    pub marked: bool,
    /// Optional short reason captured at mark time. Ignored
    /// on clear. Capped at `NOTE_BRIDGE_MAX_LEN`.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InvestigationResp {
    pub task_id: String,
    /// `Some(unix_secs)` after a mark, `None` after a clear.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marked_at: Option<i64>,
}

/// `POST /v1/tasks/:id/investigation` — toggle the operator-set
/// investigation marker. Validates the reason text bridge-side,
/// forwards to the Coordinator's `task.mark_investigation`
/// capability, and records into the intervention audit ring.
pub async fn investigation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<InvestigationReq>,
) -> Result<Json<InvestigationResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let Some(rec) = state.task_recorder.as_ref() else {
        state.intervention_audit.record_with_id(
            "anon",
            "task_investigation_set",
            &id,
            "error",
            "no coordinator",
            corr,
        );
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    // Bridge-side reason validation — mirror the note rules.
    let trimmed_reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(r) = trimmed_reason {
        if r.len() > NOTE_BRIDGE_MAX_LEN {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: format!(
                        "reason: too long (max {NOTE_BRIDGE_MAX_LEN} bytes, got {})",
                        r.len()
                    ),
                }),
            ));
        }
        if r.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: "reason: must not contain control characters".into(),
                }),
            ));
        }
    }
    let body = match rec
        .mark_investigation(&id, req.marked, trimmed_reason)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "task_investigation_set",
                &id,
                "error",
                format!("coord task.mark_investigation failed: {e}"),
                corr,
            );
            return Err((gateway_status_for(&e), Json(ApiError { error: e })));
        }
    };
    let marked_at: Option<i64> = body
        .lines()
        .find_map(|l| l.strip_prefix("marked_at="))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok());
    let detail = if req.marked {
        match trimmed_reason {
            Some(r) => format!("marked · {r}"),
            None => "marked".to_string(),
        }
    } else {
        "cleared".to_string()
    };
    state.intervention_audit.record_with_id(
        "anon",
        "task_investigation_set",
        &id,
        "ok",
        detail,
        corr,
    );
    Ok(Json(InvestigationResp {
        task_id: id,
        marked_at,
    }))
}

fn parse_failure_class_from_body(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.strip_prefix("last_failure_class="))
        .map(|v| v.trim().to_string())
}

fn parse_retry_body(body: &str) -> RetryResponse {
    let trimmed = body.trim();
    // accepted attempt=N of_budget=M
    if let Some(rest) = trimmed.strip_prefix("accepted ") {
        let mut attempt = None;
        let mut budget = None;
        for tok in rest.split_whitespace() {
            if let Some(v) = tok.strip_prefix("attempt=") {
                attempt = v.parse().ok();
            } else if let Some(v) = tok.strip_prefix("of_budget=") {
                budget = v.parse().ok();
            }
        }
        return RetryResponse {
            outcome: "accepted".to_string(),
            detail: trimmed.to_string(),
            attempt,
            of_budget: budget,
            last_failure_class: None,
        };
    }
    // exhausted retry_count=N budget=M
    if let Some(rest) = trimmed.strip_prefix("exhausted ") {
        let mut count = None;
        let mut budget = None;
        for tok in rest.split_whitespace() {
            if let Some(v) = tok.strip_prefix("retry_count=") {
                count = v.parse().ok();
            } else if let Some(v) = tok.strip_prefix("budget=") {
                budget = v.parse().ok();
            }
        }
        return RetryResponse {
            outcome: "exhausted".to_string(),
            detail: trimmed.to_string(),
            attempt: count,
            of_budget: budget,
            last_failure_class: None,
        };
    }
    // Anything else (unexpected) — return as a generic body.
    RetryResponse {
        outcome: "unknown".to_string(),
        detail: trimmed.to_string(),
        attempt: None,
        of_budget: None,
        last_failure_class: None,
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct EventsQuery {
    /// Return only events with `event_id > since`. Defaults to 0
    /// (read from the beginning).
    #[serde(default)]
    pub since: Option<i64>,
    /// Cap the response. Clamped by the Coordinator. Defaults to 200.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Exact-match filter on `event_type`. Empty / absent =
    /// no filter.
    #[serde(default)]
    pub r#type: Option<String>,
    /// `asc` (default) or `desc`. Desc gives "tail N" semantics.
    #[serde(default)]
    pub order: Option<String>,
}

/// `GET /v1/tasks/:id/events?since=N&limit=M&type=...&order=...`
/// — incremental chronicle fetch. Long-poll-friendly: read once
/// with `since=0`, remember the largest id, poll again with that
/// id to fetch only new events. Optional event-type filter and
/// order. Bridge stays translation-only: every filter / order /
/// limit is just a passthrough into the Coordinator's wire
/// format.
pub async fn events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Vec<TaskEvent>>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let after = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(200);
    let event_type = q.r#type.as_deref().unwrap_or("");
    let order = q.order.as_deref().unwrap_or("");
    let body = rec
        .events_filtered(&id, after, limit, event_type, order)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_events_lines(&body)))
}

/// One execution edge from `task.edges`. Phase-1E primitive.
/// Today only `retried_from` is emitted; other edge types are
/// reserved in the Coordinator schema.
#[derive(Debug, Serialize)]
pub struct TaskExecutionEdge {
    pub edge_id: i64,
    pub edge_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_attempt_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_by_event_id: Option<i64>,
    pub created_at: i64,
}

/// One-call full reconstruction: task detail + attempts + summary
/// plus execution edges. Returns the same shapes the per-resource
/// endpoints do, packed into one round-trip so dashboard
/// initial-render doesn't need four separate fetches.
#[derive(Debug, Serialize)]
pub struct TaskLineage {
    pub task: TaskDetail,
    pub summary: TaskSummary,
    pub attempts: Vec<TaskAttempt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<TaskExecutionEdge>,
}

/// `GET /v1/tasks/:id/lineage` — single-round-trip view of a task.
/// Each component is fetched serially via the existing capabilities
/// (no batching at the Coordinator); the win is at the HTTP layer
/// (one TLS handshake, one CORS preflight, one JSON parse).
///
/// If a component fails (e.g. older Coordinator without
/// `task.attempts`), the lineage is still returned with the other
/// components populated and the failing component's slot left
/// empty. Operator dashboards then degrade gracefully.
pub async fn lineage(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskLineage>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    // task.get is the mandatory component — if it fails we surface
    // the failure (vs degrading silently to an empty task).
    let body = rec
        .get(&id)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    let task = parse_task_body(&id, &body);
    let summary = derive_summary(&id, &body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: "coordinator returned a task body without status".into(),
        }),
    ))?;
    // Attempts is best-effort: degrade gracefully.
    let attempts = match rec.attempts(&id).await {
        Ok(s) => parse_attempts(&s),
        Err(_) => Vec::new(),
    };
    // Edges is best-effort too: older Coordinators without
    // task.edges should not break the lineage response.
    let edges = match rec.edges(&id).await {
        Ok(s) => parse_edges(&s),
        Err(_) => Vec::new(),
    };
    Ok(Json(TaskLineage {
        task,
        summary,
        attempts,
        edges,
    }))
}

/// Parse the tab-delimited body returned by `task.edges`.
/// One edge per non-empty line; columns:
///   edge_id, edge_type, attempt_id|-, related_task_id|-,
///   related_attempt_id|-, spawned_by_event_id|-, created_at
fn parse_edges(body: &str) -> Vec<TaskExecutionEdge> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 7 {
            continue;
        }
        let edge_id = match cols[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let parse_opt_i64 =
            |s: &str| -> Option<i64> { if s == "-" { None } else { s.parse().ok() } };
        let parse_opt_str =
            |s: &str| -> Option<String> { if s == "-" { None } else { Some(s.to_string()) } };
        let created_at: i64 = match cols[6].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push(TaskExecutionEdge {
            edge_id,
            edge_type: cols[1].to_string(),
            attempt_id: parse_opt_i64(cols[2]),
            related_task_id: parse_opt_str(cols[3]),
            related_attempt_id: parse_opt_i64(cols[4]),
            spawned_by_event_id: parse_opt_i64(cols[5]),
            created_at,
        });
    }
    out
}

#[derive(Debug, Serialize)]
pub struct RecentEdge {
    pub edge_id: i64,
    pub edge_type: String,
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_attempt_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_by_event_id: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct RecentEdgesQuery {
    #[serde(default)]
    pub since_edge_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parse the `task.recent_edges` body. Same column layout as
/// parse_edges but with `task_id` as the third column (after
/// edge_id + edge_type) so cross-task consumers don't need a
/// second lookup.
fn parse_recent_edges(body: &str) -> Vec<RecentEdge> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 8 {
            continue;
        }
        let edge_id = match cols[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let parse_opt_i64 =
            |s: &str| -> Option<i64> { if s == "-" { None } else { s.parse().ok() } };
        let parse_opt_str =
            |s: &str| -> Option<String> { if s == "-" { None } else { Some(s.to_string()) } };
        let created_at: i64 = match cols[7].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push(RecentEdge {
            edge_id,
            edge_type: cols[1].to_string(),
            task_id: cols[2].to_string(),
            attempt_id: parse_opt_i64(cols[3]),
            related_task_id: parse_opt_str(cols[4]),
            related_attempt_id: parse_opt_i64(cols[5]),
            spawned_by_event_id: parse_opt_i64(cols[6]),
            created_at,
        });
    }
    out
}

// ── PH-DASH2: per-task todo list (Hermes todo_tool surface) ──

#[derive(Debug, Serialize, Clone)]
pub struct TodoItem {
    pub todo_id: i64,
    pub position: i64,
    pub status: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct TodoListResponse {
    pub items: Vec<TodoItem>,
    pub count: usize,
}

#[derive(Debug, Deserialize)]
pub struct TodoSetRequest {
    /// Full ordered replacement list. Empty array clears.
    pub items: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct TodoUpdateRequest {
    /// New status — `open` or `done`.
    pub status: String,
}

/// `GET /v1/tasks/:id/todos` — read the per-task todo list.
pub async fn todo_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TodoListResponse>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = rec
        .todo_list(&id)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_todo_body(&body)))
}

/// `PUT /v1/tasks/:id/todos` — replace the full todo list.
pub async fn todo_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<TodoSetRequest>,
) -> Result<Json<TodoListResponse>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = rec
        .todo_set(&id, &req.items)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    state.intervention_audit.record(
        "anon",
        "todo_set",
        &id,
        "ok",
        format!("{} item(s)", req.items.len()),
    );
    Ok(Json(parse_todo_body(&body)))
}

/// `PATCH /v1/tasks/:id/todos/:todo_id` — toggle one todo's status.
pub async fn todo_patch(
    State(state): State<AppState>,
    Path((id, todo_id)): Path<(String, i64)>,
    Json(req): Json<TodoUpdateRequest>,
) -> Result<Json<TodoItem>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = rec
        .todo_update(&id, todo_id, &req.status)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    // Single-row response shape: `<position>\t<todo_id>\t<status>\t<text>`.
    let mut parts = body.trim_end().split('\t');
    let position = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let returned_id = parts.next().and_then(|s| s.parse().ok()).unwrap_or(todo_id);
    let status = parts.next().unwrap_or("").to_string();
    let text = parts.next().unwrap_or("").to_string();
    state.intervention_audit.record(
        "anon",
        "todo_update",
        format!("{id}/{todo_id}"),
        "ok",
        format!("status→{}", req.status),
    );
    Ok(Json(TodoItem {
        todo_id: returned_id,
        position,
        status,
        text,
    }))
}

fn parse_todo_body(body: &str) -> TodoListResponse {
    let mut items = Vec::new();
    let mut explicit_count: Option<usize> = None;
    for line in body.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("count=") {
            explicit_count = rest.parse().ok();
            continue;
        }
        let mut parts = trimmed.splitn(4, '\t');
        let position = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let todo_id = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let status = parts.next().unwrap_or("").to_string();
        let text = parts.next().unwrap_or("").to_string();
        if todo_id == 0 && text.is_empty() {
            continue;
        }
        items.push(TodoItem {
            todo_id,
            position,
            status,
            text,
        });
    }
    let count = explicit_count.unwrap_or(items.len());
    TodoListResponse { items, count }
}

// ── H6: stuck-running projection ─────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct StuckQuery {
    /// Stuck-threshold in seconds. Defaults to 300 (5 minutes)
    /// when omitted. The threshold is forwarded to the coord
    /// capability; values <0 are rejected there.
    #[serde(default)]
    pub threshold_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct StuckTask {
    pub task_id: String,
    pub title: String,
    pub started_at: i64,
    pub age_secs: i64,
}

#[derive(Debug, Serialize)]
pub struct StuckResponse {
    pub items: Vec<StuckTask>,
    pub count: usize,
    pub threshold_secs: i64,
}

/// `GET /v1/tasks/stuck?threshold_secs=N` — H6 diagnostic
/// projection. Lists tasks that are `running`, have no
/// `max_runtime_secs` (so the recovery scan can't reach them),
/// and have been running longer than the threshold. Pure read.
pub async fn stuck(
    State(state): State<AppState>,
    Query(q): Query<StuckQuery>,
) -> Result<Json<StuckResponse>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let threshold = q.threshold_secs.unwrap_or(300).max(0);
    let body = rec
        .stuck(threshold)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_stuck_body(threshold, &body)))
}

fn parse_stuck_body(threshold: i64, body: &str) -> StuckResponse {
    let mut items: Vec<StuckTask> = Vec::new();
    let mut explicit_count: Option<usize> = None;
    for line in body.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("count=") {
            explicit_count = rest.parse().ok();
            continue;
        }
        let mut parts = trimmed.splitn(4, '\t');
        let task_id = parts.next().unwrap_or("").to_string();
        let title = parts.next().unwrap_or("").to_string();
        let started_at = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let age_secs = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if task_id.is_empty() {
            continue;
        }
        items.push(StuckTask {
            task_id,
            title,
            started_at,
            age_secs,
        });
    }
    let count = explicit_count.unwrap_or(items.len());
    StuckResponse {
        items,
        count,
        threshold_secs: threshold,
    }
}

/// `GET /v1/tasks/edges/recent?since_edge_id=N&limit=M` —
/// cross-task execution edges, newest-first. Phase-1E M39
/// surface. Operators use this to spot patterns ("retry
/// storm on task X") across the runtime.
pub async fn recent_edges(
    State(state): State<AppState>,
    Query(q): Query<RecentEdgesQuery>,
) -> Result<Json<Vec<RecentEdge>>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let since = q.since_edge_id.unwrap_or(0);
    let limit = q.limit.unwrap_or(50).min(500);
    let body = rec
        .recent_edges(since, limit)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_recent_edges(&body)))
}

/// `GET /v1/tasks/:id/edges` — list execution edges that
/// touch the given task. Phase-1E M38 surface; today only
/// `retried_from` is populated.
pub async fn edges(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<TaskExecutionEdge>>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = rec
        .edges(&id)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_edges(&body)))
}

// ── M67: cross-task event firehose ─────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct RecentEventsQuery {
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Exact-match event_type filter. None / empty returns
    /// every event_type.
    #[serde(default)]
    pub event_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GlobalEventRow {
    pub task_id: String,
    pub event_id: i64,
    pub ts: i64,
    pub event_type: String,
    pub payload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<serde_json::Value>,
    /// H2: Hermes-style one-line summary derived purely from the
    /// fields above. UI projection — the chronicle row is still
    /// the source of truth.
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct GlobalEventsResponse {
    pub items: Vec<GlobalEventRow>,
    /// Newest event_id returned, suitable as the next
    /// `?since=` cursor. Empty pages echo the caller's
    /// cursor unchanged so polling clients don't reset.
    pub next_cursor: i64,
}

/// `GET /v1/tasks/events/recent?since=N&limit=N&event_type=foo`
/// — cross-task event firehose. Newest-first. Operators
/// poll this for a global runtime tail.
pub async fn recent_events(
    State(state): State<AppState>,
    Query(q): Query<RecentEventsQuery>,
) -> Result<Json<GlobalEventsResponse>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(100).min(500);
    let body = rec
        .recent_events(since, limit, q.event_type.as_deref())
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_global_events_body(since, &body)))
}

/// `GET /v1/tasks/events/stream?since=N&event_type=foo`
/// — global execution firehose as a long-lived SSE stream
/// (M73). Polls `task.recent_events` internally and emits
/// one `event` SSE frame per new chronicle entry across all
/// tasks. Each data body is the same JSON line shape the
/// `/recent` endpoint returns (task_id + event envelope).
///
/// Cursor recovery: pass `?since=N` to resume from a known
/// `event_id`. The first poll fetches everything strictly
/// newer and walks forward.
///
/// Dropped-event accounting: when more than 500 events
/// arrived between two polls (the underlying limit cap),
/// the cursor still advances to the newest seen — but a
/// `dropped` SSE frame is emitted carrying the count
/// elided so dashboards can warn the operator that the
/// global tail outran them.
///
/// Filtering: `?event_type=` is exact-match server-side
/// against the coord capability.
pub async fn events_stream_global(
    State(state): State<AppState>,
    Query(q): Query<RecentEventsQuery>,
) -> Result<
    Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, Json<ApiError>),
> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let rec = rec.clone();
    let initial_since = q.since.unwrap_or(0);
    let event_type = q.event_type.clone();
    // Page size for each poll. Bounded so a runaway runtime
    // can't OOM the stream; drop-account when exceeded.
    const STREAM_PAGE_LIMIT: usize = 500;
    // Register the stream against a synthetic task_id so the
    // existing /v1/streams panel counts the firehose too.
    let opened_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let stream_guard = state
        .stream_metrics
        .open("__firehose__".to_string(), opened_at);
    let s = stream! {
        let _live_guard = stream_guard;
        let mut since = initial_since;
        loop {
            match rec
                .recent_events(since, STREAM_PAGE_LIMIT, event_type.as_deref())
                .await
            {
                Ok(body) => {
                    // Coord returns newest-first; emit oldest-
                    // first so the operator's view reads
                    // chronologically. Collect, reverse, emit.
                    let mut lines: Vec<&str> = body
                        .lines()
                        .filter(|l| !l.is_empty())
                        .collect();
                    lines.reverse();
                    let page_size = lines.len();
                    let mut newest_in_page = since;
                    for line in lines {
                        // Advance cursor via the same prefix
                        // scan the per-task stream uses, except
                        // the global rows carry `task_id` as
                        // the first field. We just look for
                        // the `"id":N` substring.
                        if let Some(id_field) = extract_event_id_prefix(line)
                            && id_field > newest_in_page
                        {
                            newest_in_page = id_field;
                        }
                        // H2: enrich the SSE line with the same
                        // `summary` projection /recent carries so
                        // dashboard consumers see one shape from
                        // both code paths. Tolerant of unexpected
                        // input — non-JSON or malformed lines
                        // pass through unchanged.
                        let enriched = enrich_stream_line_with_summary(line);
                        yield Ok(Event::default().event("event").data(enriched));
                    }
                    if newest_in_page > since {
                        since = newest_in_page;
                    }
                    // Drop accounting: if the page came back
                    // full AND the page advanced the cursor by
                    // more than STREAM_PAGE_LIMIT events, the
                    // stream fell behind. We can't know the
                    // exact count without a second query, but
                    // we surface that we hit the cap so the
                    // dashboard can warn.
                    if page_size >= STREAM_PAGE_LIMIT {
                        let dropped_payload = serde_json::json!({
                            "page_size": page_size,
                            "next_cursor": since,
                            "note": "firehose page hit STREAM_PAGE_LIMIT; \
                                     older events may have been elided. \
                                     Re-poll /v1/tasks/events/recent with \
                                     since=<prior_cursor> for a precise count."
                        })
                        .to_string();
                        yield Ok(Event::default()
                            .event("dropped")
                            .data(dropped_payload));
                    }
                }
                Err(e) => {
                    // Transient — emit but keep the stream
                    // alive. Operator dashboards may surface;
                    // gateway tooling can throttle on repeated
                    // errors.
                    yield Ok(Event::default().event("error").data(e));
                }
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
    };
    Ok(Sse::new(s).keep_alive(KeepAlive::default()))
}

/// TG5 — map a chronicle event `type` to the normalized SSE event
/// name the run/Brief execution stream emits. Returns `None` for any
/// type that isn't an execution transition (so the stream stays
/// run-focused). The set agrees with
/// `relix_runtime::nodes::coordinator::RUN_STREAM_EVENT_TYPES` — the
/// coordinator already filters to these, this just relabels.
fn run_stream_event_name(chronicle_type: &str) -> Option<&'static str> {
    match chronicle_type {
        "brief.run_started" => Some("run_started"),
        "brief.shift_done"
        | "brief.dispatch_failed"
        | "brief.continued"
        | "brief.run_recovered"
        | "brief.run_refused" => Some("run_finished"),
        "brief.run_cancel_requested" => Some("run_cancel_requested"),
        "brief.board_moved" => Some("brief_moved"),
        "brief.run_reviewed" => Some("review_changed"),
        "brief.run_applied" | "brief.run_discarded" => Some("apply_changed"),
        _ => None,
    }
}

/// `GET /v1/runs/events/stream?since=N` — the run/Brief EXECUTION
/// event stream (TG5) as a long-lived SSE connection. Polls the
/// tenant-scoped `run.events.recent` coord capability and emits one
/// SSE frame per execution transition, with the frame's `event:`
/// field set to the normalized name (`run_started`, `run_finished`,
/// `run_cancel_requested`, `brief_moved`, `review_changed`,
/// `apply_changed`). Auth + tenant are enforced by the `/v1/*`
/// middleware; the resolved tenant is captured here and re-applied on
/// every poll so a long-lived stream stays scoped to the caller's
/// Guild (the body is polled outside the middleware's task-local
/// scope). `keep_alive` emits periodic comment pings so idle streams
/// and proxies stay open.
///
/// Cursor recovery: `?since=N` resumes strictly after `event_id` N.
pub async fn runs_events_stream(
    State(state): State<AppState>,
    Query(q): Query<RecentEventsQuery>,
) -> Result<
    Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, Json<ApiError>),
> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let rec = rec.clone();
    let initial_since = q.since.unwrap_or(0);
    // Capture the resolved tenant NOW (inside the middleware scope) so
    // the stream body — polled later, outside that scope — re-applies it
    // on each coord call. `default` in single-tenant mode.
    let tenant_scope = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    const STREAM_PAGE_LIMIT: usize = 500;
    let opened_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let stream_guard = state
        .stream_metrics
        .open("__run_events__".to_string(), opened_at);
    let s = stream! {
        let _live_guard = stream_guard;
        let mut since = initial_since;
        loop {
            // Re-establish the tenant task-local for this poll so the
            // outbound envelope carries the caller's tenant.
            let fetch = CURRENT_TENANT.scope(
                tenant_scope.clone(),
                rec.run_events_recent(since, STREAM_PAGE_LIMIT),
            );
            match fetch.await {
                Ok(body) => {
                    // Coord returns newest-first; emit oldest-first.
                    let mut lines: Vec<&str> = body
                        .lines()
                        .filter(|l| !l.is_empty())
                        .collect();
                    lines.reverse();
                    let mut newest_in_page = since;
                    for line in lines {
                        if let Some(id_field) = extract_event_id_prefix(line)
                            && id_field > newest_in_page
                        {
                            newest_in_page = id_field;
                        }
                        // Only forward recognized execution transitions,
                        // labeled with the normalized SSE event name.
                        let ev_type = serde_json::from_str::<serde_json::Value>(line)
                            .ok()
                            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string));
                        if let Some(t) = ev_type
                            && let Some(name) = run_stream_event_name(&t)
                        {
                            let enriched = enrich_stream_line_with_summary(line);
                            yield Ok(Event::default().event(name).data(enriched));
                        }
                    }
                    if newest_in_page > since {
                        since = newest_in_page;
                    }
                }
                Err(e) => {
                    yield Ok(Event::default().event("error").data(e));
                }
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
    };
    Ok(Sse::new(s).keep_alive(KeepAlive::default().text("ping")))
}

/// H2: add a `summary` field to an event line emitted by coord
/// before forwarding it through the SSE stream. If the line is
/// non-JSON or missing fields, it passes through unchanged so the
/// stream stays resilient against unexpected coord additions.
fn enrich_stream_line_with_summary(line: &str) -> String {
    let mut v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return line.to_string(),
    };
    let event_type = v
        .get("type")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if event_type.is_empty() {
        return line.to_string();
    }
    let payload = v
        .get("payload")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let attempt_id = v.get("attempt_id").and_then(|x| x.as_i64());
    let pj_str = v.get("payload_json").map(|j| j.to_string());
    let summary = relix_runtime::nodes::coordinator::summarize_event_parts(
        &event_type,
        &payload,
        attempt_id,
        pj_str.as_deref(),
    );
    if let Some(obj) = v.as_object_mut() {
        obj.insert("summary".into(), serde_json::Value::String(summary));
    }
    v.to_string()
}

/// Parse one JSON object per line into a typed envelope.
/// Tolerant of partial-shape rows — anything failing to
/// deserialize is silently skipped (the firehose stays
/// resilient against unexpected runtime additions).
fn parse_global_events_body(since: i64, body: &str) -> GlobalEventsResponse {
    let mut items: Vec<GlobalEventRow> = Vec::new();
    let mut newest = since;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let task_id = match v.get("task_id").and_then(|x| x.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let event_id = match v.get("id").and_then(|x| x.as_i64()) {
            Some(i) => i,
            None => continue,
        };
        let ts = v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0);
        let event_type = v
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let payload = v
            .get("payload")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let attempt_id = v.get("attempt_id").and_then(|x| x.as_i64());
        let trace_id = v
            .get("trace_id")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        let payload_json = v.get("payload_json").cloned();
        if event_id > newest {
            newest = event_id;
        }
        // H2: cheap one-line summary projection. Serialise the
        // payload_json back to a string so the summarizer can
        // do its best-effort typed pattern match against it.
        let pj_str = payload_json.as_ref().map(|j| j.to_string());
        let summary = relix_runtime::nodes::coordinator::summarize_event_parts(
            &event_type,
            &payload,
            attempt_id,
            pj_str.as_deref(),
        );
        items.push(GlobalEventRow {
            task_id,
            event_id,
            ts,
            event_type,
            payload,
            attempt_id,
            trace_id,
            payload_json,
            summary,
        });
    }
    GlobalEventsResponse {
        items,
        next_cursor: newest,
    }
}

// ── M66: execution-lineage traversal ───────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct LineageQuery {
    #[serde(default)]
    pub depth: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct LineageEdge {
    pub edge_id: i64,
    pub edge_type: String,
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_task_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct LineageResponse {
    pub root_task_id: String,
    pub tasks: Vec<String>,
    pub edges: Vec<LineageEdge>,
    pub cross_task_edge_count: usize,
    pub max_depth_walked: usize,
    /// Honest note — bridge-supplied, surfaces the runtime
    /// gap to operators consuming the JSON directly (not
    /// just via the dashboard). Always present.
    pub note: String,
}

/// `GET /v1/tasks/:id/lineage_graph?depth=N` — BFS execution
/// lineage. Returns the set of related tasks + the edges
/// connecting them. Today only `retried_from` populates the
/// graph (intra-task only). Distinct path from
/// `/v1/tasks/:id/lineage` so the routes don't collide —
/// the single-task lineage envelope owns `/lineage` and this
/// graph view owns `/lineage_graph`.
pub async fn lineage_graph(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<LineageQuery>,
) -> Result<Json<LineageResponse>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let depth = q.depth.unwrap_or(4);
    let body = rec
        .lineage(&id, depth)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_lineage_body(&id, &body)))
}

/// Parse the multi-line tab-delimited body emitted by the
/// Coordinator's `task.lineage` handler into a typed
/// envelope. Tolerant of missing header fields (`root=`,
/// `tasks=`, `cross_task_edges=`, `max_depth=`) — fills
/// honest defaults.
fn parse_lineage_body(fallback_root: &str, body: &str) -> LineageResponse {
    let mut root = fallback_root.to_string();
    let mut tasks: Vec<String> = vec![fallback_root.to_string()];
    let mut cross_task_edge_count = 0usize;
    let mut max_depth_walked = 4usize;
    let mut edges: Vec<LineageEdge> = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("root=") {
            root = rest.trim().to_string();
            continue;
        }
        if let Some(rest) = line.strip_prefix("tasks=") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                tasks = trimmed.split(',').map(str::to_string).collect();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("cross_task_edges=") {
            cross_task_edge_count = rest.trim().parse().unwrap_or(0);
            continue;
        }
        if let Some(rest) = line.strip_prefix("max_depth=") {
            max_depth_walked = rest.trim().parse().unwrap_or(max_depth_walked);
            continue;
        }
        // Edge row: edge_id\tedge_type\ttask_id\trelated_task_id\tcreated_at
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 5 {
            continue;
        }
        let Ok(edge_id) = parts[0].parse::<i64>() else {
            continue;
        };
        let related = if parts[3] == "-" || parts[3].is_empty() {
            None
        } else {
            Some(parts[3].to_string())
        };
        let Ok(created_at) = parts[4].parse::<i64>() else {
            continue;
        };
        edges.push(LineageEdge {
            edge_id,
            edge_type: parts[1].to_string(),
            task_id: parts[2].to_string(),
            related_task_id: related,
            created_at,
        });
    }
    let note = if cross_task_edge_count == 0 {
        // Honest scope note that the dashboard surfaces too.
        "no cross-task edges recorded yet — only retried_from \
         producers ship today, and they link a task to itself; \
         spawned/delegated_to/parallel_branch/blocked_on/awaited/\
         resumed_from remain reserved in the schema until \
         runtime primitives emit them."
            .to_string()
    } else {
        format!(
            "{cross_task_edge_count} cross-task edge(s) recorded \
             across {} related task(s); only retried_from has a \
             producer today, so cross-task edges are unusual but \
             genuine when present.",
            tasks.len().saturating_sub(1)
        )
    };
    LineageResponse {
        root_task_id: root,
        tasks,
        edges,
        cross_task_edge_count,
        max_depth_walked,
        note,
    }
}

/// **Experimental** SSE wrapper around `task.events` polling.
///
/// `GET /v1/tasks/:id/events/stream?since=N` opens an SSE stream
/// that emits one `Event` per chronicle event newer than `since`.
/// The bridge polls the Coordinator's `task.events` capability
/// internally — it owns no per-stream state beyond the cursor on
/// the client's open socket, which dies with the socket.
///
/// Each SSE message carries the same JSON envelope shape
/// `/v1/tasks/:id/events` returns. Consumers `JSON.parse` each
/// message body without further structure.
///
/// On Coordinator NotFound, the stream emits a terminal `event:
/// gone` message and closes. On other errors, the stream sleeps
/// the poll interval and retries — transient Coordinator outages
/// don't kill long-lived dashboards.
///
/// **Bridge invariant:** zero task-state ownership. The stream
/// is a presentation wrapper over the same polling loop the CLI
/// `task watch` uses. If SSE turns out to be invasive or
/// resource-heavy at scale we delete this endpoint; cursor +
/// typed events remain the supported surface.
pub async fn events_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<
    Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, Json<ApiError>),
> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let rec = rec.clone();
    let initial_after = q.since.unwrap_or(0);
    let event_type = q.r#type.clone().unwrap_or_default();
    let order = q.order.clone().unwrap_or_default();
    let id_for_stream = id;
    // RAII guard: registers this stream against the task_id
    // and decrements active + removes the detail entry on
    // drop. Drop fires when the stream's future is cancelled
    // (client disconnect) OR when the stream exits normally
    // (terminal `gone` event).
    let opened_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let stream_guard = state.stream_metrics.open(id_for_stream.clone(), opened_at);
    let s = stream! {
        // Hold the guard inside the stream body so it lives
        // exactly as long as the stream itself.
        let _live_guard = stream_guard;
        let mut after = initial_after;
        loop {
            match rec.events_filtered(&id_for_stream, after, 200, &event_type, &order).await {
                Ok(body) => {
                    for line in body.lines() {
                        if line.is_empty() {
                            continue;
                        }
                        // Advance cursor via a prefix-scan on
                        // the leading `{"id":N,` rather than a
                        // full serde_json parse. A line whose
                        // payload contains hostile chars could
                        // fail full-JSON parse — but its `id`
                        // prefix is fixed-shape per
                        // render_event_json. Without this, a
                        // single un-parseable line would stop
                        // cursor advancement and the next poll
                        // would re-deliver every event from the
                        // last good cursor, duplicating
                        // forever.
                        if let Some(id_field) = extract_event_id_prefix(line)
                            && id_field > after
                        {
                            after = id_field;
                        }
                        // Emit the raw JSON line as the SSE data body.
                        yield Ok(Event::default().event("event").data(line));
                    }
                }
                Err(e) if e.contains("not found") => {
                    yield Ok(Event::default().event("gone").data(e));
                    break;
                }
                Err(e) => {
                    // Transient error — surface it but keep the
                    // stream alive. Operator dashboards may want
                    // to surface this; gateway tooling can throttle
                    // on repeated errors.
                    yield Ok(Event::default().event("error").data(e));
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    Ok(Sse::new(s).keep_alive(KeepAlive::default()))
}

/// `GET /v1/tasks/:id/export` — archival snapshot for operator
/// download. Returns the Coordinator's single-JSON artifact
/// verbatim with `Content-Disposition: attachment` so browsers
/// save directly to disk.
///
/// This is the "save-before-delete" path the chronicle
/// retention design requires before destructive deletion lands.
pub async fn export(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = rec
        .export(&id)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    let filename = format!("task-{id}.json");
    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(axum::body::Body::from(body))
        .expect("export response builds"))
}

/// `GET /v1/tasks/compact_events?max_age_secs=N` —
/// chronicle-retention dry-run candidate counter.
///
/// Returns the Coordinator's `task.compact_events` JSON
/// verbatim. No deletion happens; this is the operator's
/// planning surface for the eventual Step 3 destructive
/// pass. `max_age_secs` is required and must be a positive
/// integer. The `mode` is hard-coded to `dry-run` at the
/// bridge — when Step 3 lands and adds a `delete` mode, that
/// will land here as a separate `POST` endpoint with stricter
/// guards (operator confirmation, separate capability, etc.),
/// not as a query parameter on this read-only path.
pub async fn compact_events_dry_run(
    State(state): State<AppState>,
    Query(q): Query<CompactQuery>,
) -> Result<axum::response::Response, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let max_age_secs = q.max_age_secs.unwrap_or(0);
    if max_age_secs <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "max_age_secs query param required (positive integer)".into(),
            }),
        ));
    }
    let body = rec
        .compact_events_dry_run(max_age_secs)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("compact response builds"))
}

#[derive(Debug, Deserialize, Default)]
pub struct CompactQuery {
    #[serde(default)]
    pub max_age_secs: Option<i64>,
}

pub async fn attempts(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<TaskAttempt>>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    if !is_valid_task_id(&id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "task_id must be 32 hex chars".into(),
            }),
        ));
    }
    let body = rec
        .attempts(&id)
        .await
        .map_err(|e| (gateway_status_for(&e), Json(ApiError { error: e })))?;
    Ok(Json(parse_attempts(&body)))
}

fn no_coordinator() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "coordinator not configured ([coordinator] alias missing)".into(),
        }),
    )
}

fn record_task_control_activity(
    state: &AppState,
    action: &str,
    task_id: Option<&str>,
    target: &str,
    decision: &str,
    detail: impl Into<String>,
) {
    let tenant = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "anon".to_string());
    let detail = detail.into();
    if let Err(e) = append_task_control_activity(
        state.cfg.transport.data_dir.as_deref(),
        TaskControlActivity {
            tenant_id: &tenant,
            actor: &actor,
            action,
            task_id,
            run_id: None,
            target,
            decision,
            detail: &detail,
        },
    ) {
        tracing::warn!(
            action,
            target,
            error = %e,
            "task control activity append failed"
        );
    }
}

/// Distinguish "not found" from generic gateway errors when the
/// Coordinator cause string indicates it. Keeps the 404 path correct
/// without requiring a wire-format change.
fn gateway_status_for(cause: &str) -> StatusCode {
    if cause.contains("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_GATEWAY
    }
}

fn is_valid_task_id(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Parse a `task.get` body (key=value lines + `events=[JSON array]`)
/// into a `TaskDetail`. Robust against unknown header keys — they
/// are passed through as-is so future Coordinator additions surface
/// without bridge changes.
fn parse_task_body(id: &str, raw: &str) -> TaskDetail {
    let mut header = BTreeMap::new();
    let mut events_line: Option<&str> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("events=") {
            events_line = Some(rest);
            continue;
        }
        if let Some(eq) = line.find('=') {
            let (k, v) = line.split_at(eq);
            header.insert(k.to_string(), v[1..].to_string());
        }
    }
    let events = events_line.map(parse_events_array).unwrap_or_default();
    TaskDetail {
        task_id: id.to_string(),
        header,
        events,
    }
}

/// Parse the Coordinator's JSON event array using `serde_json`.
/// Switched from a hand-rolled brace-counter (which couldn't
/// nest) to proper JSON parsing once events started carrying
/// `payload_json` objects (S2). Malformed input still returns an
/// empty Vec so a corrupted chronicle doesn't fail the whole
/// request.
fn parse_events_array(s: &str) -> Vec<TaskEvent> {
    serde_json::from_str::<Vec<RawEvent>>(s.trim())
        .map(|raws| raws.into_iter().map(RawEvent::into_task_event).collect())
        .unwrap_or_default()
}

fn parse_event_object(obj: &str) -> Option<TaskEvent> {
    serde_json::from_str::<RawEvent>(obj)
        .ok()
        .map(RawEvent::into_task_event)
}

/// Wire shape the Coordinator emits — distinct from the
/// outbound `TaskEvent` so we can do field renames at the
/// boundary (id → event_id, type → event_type) without leaking
/// the wire keys into the public JSON contract.
#[derive(Debug, Deserialize)]
struct RawEvent {
    id: i64,
    ts: i64,
    r#type: String,
    payload: String,
    #[serde(default)]
    schema_version: i64,
    #[serde(default)]
    attempt_id: Option<i64>,
    #[serde(default)]
    trace_id: Option<String>,
    #[serde(default)]
    payload_json: Option<serde_json::Value>,
}

impl RawEvent {
    fn into_task_event(self) -> TaskEvent {
        TaskEvent {
            event_id: self.id,
            ts: self.ts,
            event_type: self.r#type,
            payload: self.payload,
            schema_version: self.schema_version,
            attempt_id: self.attempt_id,
            trace_id: self.trace_id,
            payload_json: self.payload_json,
        }
    }
}

/// Derive a [`TaskSummary`] from a parsed `task.get` body. Same
/// logic the CLI's `--pretty` summary line uses; the two surfaces
/// stay in sync because both consume the Coordinator's
/// `key=value` projection.
///
/// Returns `None` when the body lacks `status=` — which never
/// happens for a real Coordinator response but the JSON contract
/// is honest about it.
fn derive_summary(id: &str, raw: &str) -> Option<TaskSummary> {
    let mut status: Option<&str> = None;
    let mut attempt_count: Option<i64> = None;
    let mut started_at: Option<i64> = None;
    let mut updated_at: Option<i64> = None;
    let mut last_failure_class: Option<String> = None;
    let mut last_failure_reason: Option<String> = None;
    let mut retry_policy: Option<String> = None;
    let mut retry_count: Option<i64> = None;
    let mut max_retries: Option<i64> = None;
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("status=") {
            status = Some(v);
        } else if let Some(v) = line.strip_prefix("attempt_count=") {
            attempt_count = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("started_at=") {
            started_at = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("updated_at=") {
            updated_at = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("last_failure_class=") {
            last_failure_class = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("last_failure_reason=") {
            last_failure_reason = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("retry_policy=") {
            retry_policy = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("retry_count=") {
            retry_count = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("max_retries=") {
            max_retries = v.parse().ok();
        }
    }
    let status_s = status?.to_string();
    let duration_secs = match (status_s.as_str(), started_at, updated_at) {
        ("completed" | "failed" | "cancelled" | "interrupted", Some(s), Some(u)) if u >= s => {
            Some(u - s)
        }
        _ => None,
    };
    // Render the retries field the same way the CLI does, but only
    // when the policy is non-`none`.
    let retries = match (retry_policy.as_deref(), max_retries) {
        (Some(p), Some(m)) if p != "none" => {
            let c = retry_count.unwrap_or(0);
            Some(format!("{c}/{m}"))
        }
        _ => None,
    };
    let retry_policy_out = match retry_policy.as_deref() {
        Some("none") | None => None,
        Some(p) => Some(p.to_string()),
    };
    Some(TaskSummary {
        task_id: id.to_string(),
        status: status_s,
        attempt_count,
        duration_secs,
        started_at,
        last_failure_class,
        last_failure_reason,
        retries,
        retry_policy: retry_policy_out,
    })
}

/// Extract the `id` field from one line of `task.events` output
/// without doing a full JSON parse. The Coordinator's
/// `render_event_json` always emits `{"id":N,...` first, so a
/// prefix scan is robust against any subsequent payload
/// content — including malformed `payload_json` blobs that
/// would break a full parse.
///
/// Returns `None` when the line doesn't start with the expected
/// prefix or the integer doesn't parse; callers leave the
/// cursor untouched in that case (the line still emits to the
/// SSE client; transport-layer reconnect with `?since=` from
/// the client's own state recovers).
fn extract_event_id_prefix(line: &str) -> Option<i64> {
    let rest = line.strip_prefix("{\"id\":")?;
    let comma = rest.find(',')?;
    rest[..comma].parse().ok()
}

/// Parse a `task.events` body — one JSON event per line.
/// Tolerant of empty lines and malformed entries (which are
/// skipped silently).
fn parse_events_lines(body: &str) -> Vec<TaskEvent> {
    body.lines()
        .filter(|l| !l.is_empty())
        .filter_map(parse_event_object)
        .collect()
}

/// Parse a `task.list_cursor` body: tab-delimited
/// `task_id\tstatus\ttitle\tupdated_at\tinvestigation_marked_at`
/// rows followed by a trailing `next_cursor=<value>\n`.
/// Returns the rows + the optional cursor (None on empty
/// value). 4-column and 3-column legacy shapes are still
/// accepted (the trailing columns default to None) so the
/// dashboard keeps working against an older Coordinator that
/// hasn't picked up the M63 schema.
fn parse_cursor_body(body: &str) -> (Vec<TaskListEntry>, Option<String>) {
    let mut items = Vec::new();
    let mut next: Option<String> = None;
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("next_cursor=") {
            if rest.is_empty() {
                next = None;
            } else {
                next = Some(rest.to_string());
            }
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, '\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let updated_at = parts.get(3).and_then(|v| v.trim().parse::<i64>().ok());
        let investigation_marked_at = parts
            .get(4)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .and_then(|v| v.parse::<i64>().ok());
        items.push(TaskListEntry {
            task_id: parts[0].to_string(),
            status: parts[1].to_string(),
            title: parts[2].to_string(),
            updated_at,
            investigation_marked_at,
        });
    }
    (items, next)
}

/// Parse the `task.count` body — a single line `count=N`.
/// Tolerant of trailing whitespace / newlines.
fn parse_count_body(body: &str) -> Option<i64> {
    body.lines()
        .find_map(|l| l.strip_prefix("count="))
        .and_then(|v| v.trim().parse().ok())
}

/// Parse the `task.recover` body: one task_id per line, then a
/// trailing `recovered=N\n`. Returns the recovered ids plus the
/// reported count (which should equal `ids.len()` but the caller
/// is the source of truth on the count, not us).
fn parse_recover_body(body: &str) -> (Vec<String>, usize) {
    let mut ids = Vec::new();
    let mut reported_count = 0usize;
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("recovered=") {
            reported_count = rest.parse().unwrap_or(0);
        } else {
            ids.push(line.to_string());
        }
    }
    (ids, reported_count)
}

/// Parse `task.attempts` body (tab-delimited lines).
fn parse_attempts(body: &str) -> Vec<TaskAttempt> {
    body.lines()
        .filter_map(|line| {
            if line.is_empty() {
                return None;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 6 {
                return None;
            }
            Some(TaskAttempt {
                attempt_num: parts[0].parse().ok()?,
                status: parts[1].to_string(),
                started_at: parts[2].parse().ok()?,
                finished_at: if parts[3] == "-" {
                    None
                } else {
                    parts[3].parse().ok()
                },
                failure_class: if parts[4] == "-" {
                    None
                } else {
                    Some(parts[4].to_string())
                },
                flow_id: if parts[5] == "-" {
                    None
                } else {
                    Some(parts[5].to_string())
                },
            })
        })
        .collect()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        Json(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_task_id_accepts_32_hex_only() {
        assert!(is_valid_task_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_task_id("short"));
        assert!(!is_valid_task_id(
            "0123456789abcdef0123456789abcdef00" // 34 chars
        ));
        assert!(!is_valid_task_id("0123456789abcdef0123456789abcdeg")); // non-hex
    }

    #[test]
    fn parse_task_body_extracts_header_and_events() {
        let raw = "task_id=abc\nstatus=completed\nretry_count=2\nevents=[{\"id\":1,\"ts\":100,\"type\":\"x\",\"payload\":\"p\"}]\n";
        let d = parse_task_body("abc", raw);
        assert_eq!(d.header.get("status").unwrap(), "completed");
        assert_eq!(d.header.get("retry_count").unwrap(), "2");
        assert_eq!(d.events.len(), 1);
        assert_eq!(d.events[0].event_type, "x");
    }

    #[test]
    fn parse_task_body_handles_empty_events() {
        let raw = "task_id=abc\nstatus=pending\nevents=[]\n";
        let d = parse_task_body("abc", raw);
        assert!(d.events.is_empty());
        assert_eq!(d.header.get("status").unwrap(), "pending");
    }

    #[test]
    fn parse_attempts_returns_typed_rows() {
        let body = "1\tfailed\t100\t105\ttransient\tflowA\n2\trunning\t110\t-\t-\t-\n";
        let rows = parse_attempts(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, "failed");
        assert_eq!(rows[0].finished_at, Some(105));
        assert_eq!(rows[0].failure_class.as_deref(), Some("transient"));
        assert_eq!(rows[1].status, "running");
        assert!(rows[1].finished_at.is_none());
        assert!(rows[1].failure_class.is_none());
        assert!(rows[1].flow_id.is_none());
    }

    #[test]
    fn summary_terminal_includes_duration_and_retries() {
        let raw = concat!(
            "task_id=abc\n",
            "status=completed\n",
            "started_at=1700000000\n",
            "updated_at=1700000007\n",
            "attempt_count=2\n",
            "retry_policy=bounded\n",
            "retry_count=1\n",
            "max_retries=3\n",
            "events=[]\n"
        );
        let s = derive_summary("abc", raw).unwrap();
        assert_eq!(s.status, "completed");
        assert_eq!(s.attempt_count, Some(2));
        assert_eq!(s.duration_secs, Some(7));
        assert_eq!(s.retries.as_deref(), Some("1/3"));
        assert_eq!(s.retry_policy.as_deref(), Some("bounded"));
    }

    #[test]
    fn summary_running_omits_duration() {
        let raw = "task_id=abc\nstatus=running\nstarted_at=1700000000\nupdated_at=1700000050\nattempt_count=1\nevents=[]\n";
        let s = derive_summary("abc", raw).unwrap();
        assert!(s.duration_secs.is_none());
        assert_eq!(s.started_at, Some(1_700_000_000));
    }

    #[test]
    fn summary_with_retry_policy_none_omits_retries_field() {
        let raw = "task_id=abc\nstatus=failed\nstarted_at=100\nupdated_at=105\nattempt_count=1\nretry_policy=none\nretry_count=0\nmax_retries=0\nlast_failure_class=permanent\nevents=[]\n";
        let s = derive_summary("abc", raw).unwrap();
        assert!(s.retries.is_none());
        assert!(s.retry_policy.is_none());
        assert_eq!(s.last_failure_class.as_deref(), Some("permanent"));
        assert_eq!(s.duration_secs, Some(5));
    }

    #[test]
    fn summary_returns_none_when_status_missing() {
        let raw = "task_id=abc\nevents=[]\n";
        assert!(derive_summary("abc", raw).is_none());
    }

    #[test]
    fn parse_recover_body_extracts_ids_and_count() {
        let body = "abc111\ndef222\nrecovered=2\n";
        let (ids, count) = parse_recover_body(body);
        assert_eq!(ids, vec!["abc111".to_string(), "def222".to_string()]);
        assert_eq!(count, 2);
    }

    #[test]
    fn extract_event_id_prefix_handles_typical_lines() {
        assert_eq!(
            extract_event_id_prefix(r#"{"id":42,"ts":1,"type":"x","payload":""}"#),
            Some(42)
        );
        assert_eq!(
            extract_event_id_prefix(
                r#"{"id":1,"ts":0,"type":"a","payload":"b","schema_version":1,"attempt_id":7,"trace_id":"abc","payload_json":{"k":"v"}}"#
            ),
            Some(1)
        );
    }

    #[test]
    fn extract_event_id_prefix_resists_malformed_payload_json() {
        // Hostile payload_json with embedded braces / quotes
        // that would break a full JSON parse. The prefix scan
        // must still find the id.
        let line = r#"{"id":99,"ts":1,"type":"x","payload":"hostile","schema_version":1,"payload_json":{"weird":"}\"with\"nested":"braces"}}"#;
        assert_eq!(extract_event_id_prefix(line), Some(99));
    }

    #[test]
    fn extract_event_id_prefix_rejects_non_event_lines() {
        assert_eq!(extract_event_id_prefix(""), None);
        assert_eq!(extract_event_id_prefix("not an event"), None);
        assert_eq!(extract_event_id_prefix(r#"{"ts":1,"id":42}"#), None); // id not first
        assert_eq!(extract_event_id_prefix(r#"{"id":notnum,"ts":1}"#), None);
    }

    #[test]
    fn parse_events_lines_handles_typical_body() {
        let body = concat!(
            r#"{"id":1,"ts":100,"type":"task.created","payload":"x"}"#,
            "\n",
            r#"{"id":2,"ts":105,"type":"flow.started","payload":"chat"}"#,
            "\n"
        );
        let out = parse_events_lines(body);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event_id, 1);
        assert_eq!(out[0].event_type, "task.created");
        assert_eq!(out[1].ts, 105);
    }

    #[test]
    fn parse_events_lines_skips_blank_and_malformed_lines() {
        let body = concat!(
            "\n",
            r#"{"id":1,"ts":100,"type":"x","payload":"y"}"#,
            "\n",
            "garbage line\n",
            r#"{"id":2,"ts":200,"type":"z","payload":""}"#,
            "\n"
        );
        let out = parse_events_lines(body);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event_id, 1);
        assert_eq!(out[1].event_id, 2);
    }

    #[test]
    fn parse_events_lines_empty_body_returns_empty() {
        assert!(parse_events_lines("").is_empty());
        assert!(parse_events_lines("\n\n").is_empty());
    }

    #[test]
    fn parse_events_lines_surfaces_typed_envelope_fields() {
        // S2: the Coordinator emits schema_version, attempt_id,
        // trace_id, payload_json on structured events. The bridge
        // surface them on TaskEvent so dashboards can consume the
        // typed payload directly.
        let body = concat!(
            r#"{"id":1,"ts":100,"type":"task.attempt_started","payload":"attempt_id=42 attempt_num=1","schema_version":1,"attempt_id":42,"trace_id":"abc","payload_json":{"attempt_id":42,"attempt_num":1}}"#,
            "\n"
        );
        let out = parse_events_lines(body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].schema_version, 1);
        assert_eq!(out[0].attempt_id, Some(42));
        assert_eq!(out[0].trace_id.as_deref(), Some("abc"));
        let pj = out[0].payload_json.as_ref().expect("payload_json present");
        assert_eq!(pj.get("attempt_num").and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn parse_events_lines_legacy_v0_still_works() {
        // Existing v0 events (no typed envelope keys) must continue
        // to parse cleanly with default schema_version=0 and
        // all-None typed fields.
        let body = r#"{"id":1,"ts":100,"type":"ops.custom","payload":"anything"}"#;
        let out = parse_events_lines(body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].schema_version, 0);
        assert!(out[0].attempt_id.is_none());
        assert!(out[0].trace_id.is_none());
        assert!(out[0].payload_json.is_none());
    }

    #[test]
    fn parse_cursor_body_extracts_rows_and_cursor() {
        let body = "abc\trunning\tt0\t100\ndef\tpending\tt1\t99\nnext_cursor=99:def\n";
        let (items, next) = parse_cursor_body(body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].task_id, "abc");
        assert_eq!(items[1].status, "pending");
        assert_eq!(items[0].updated_at, Some(100));
        assert_eq!(items[1].updated_at, Some(99));
        assert_eq!(next.as_deref(), Some("99:def"));
    }

    #[test]
    fn parse_cursor_body_tolerates_missing_updated_at_field() {
        // Older coord builds (or truncated lines) may omit the
        // fourth column. We must still parse the row, just with
        // updated_at=None — the dashboard renders "—" instead
        // of inventing an age.
        let body = "abc\trunning\tt0\nnext_cursor=\n";
        let (items, _next) = parse_cursor_body(body);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].updated_at, None);
        assert_eq!(items[0].investigation_marked_at, None);
    }

    #[test]
    fn parse_stuck_body_basic() {
        // H6: tab-separated rows with trailing count=<N>.
        let body = "deadbeef\tmy task\t1700000000\t900\n\
                    cafebabe\tother\t1699999999\t1800\n\
                    count=2\n";
        let r = parse_stuck_body(300, body);
        assert_eq!(r.count, 2);
        assert_eq!(r.threshold_secs, 300);
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].task_id, "deadbeef");
        assert_eq!(r.items[0].title, "my task");
        assert_eq!(r.items[0].started_at, 1700000000);
        assert_eq!(r.items[0].age_secs, 900);
    }

    #[test]
    fn parse_stuck_body_empty_body_count_zero() {
        let r = parse_stuck_body(300, "count=0\n");
        assert_eq!(r.count, 0);
        assert!(r.items.is_empty());
    }

    #[test]
    fn parse_stuck_body_falls_back_to_items_len_when_count_missing() {
        // Older bridge / partial fetch — still produce a useful
        // response. The trailing `count=` is convenience, not
        // load-bearing.
        let body = "abc\tt\t100\t10\n";
        let r = parse_stuck_body(60, body);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.count, 1);
    }

    #[test]
    fn parse_global_events_body_advances_cursor_to_newest_event_id() {
        // M67: each row's `id` field feeds the next_cursor.
        // Newest event id (largest in the page) wins.
        let body = r#"{"task_id":"abc","id":5,"ts":100,"type":"ops.x","payload":"p"}"#.to_string()
            + "\n"
            + r#"{"task_id":"abc","id":7,"ts":101,"type":"ops.y","payload":"q"}"#
            + "\n";
        let r = parse_global_events_body(0, &body);
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.next_cursor, 7);
    }

    #[test]
    fn parse_global_events_body_skips_malformed_lines_resiliently() {
        let body = r#"{"task_id":"abc","id":1,"ts":1,"type":"ops.x","payload":""}"#.to_string()
            + "\n"
            + "this is not json\n"
            + r#"{"missing":"task_id"}"#
            + "\n"
            + r#"{"task_id":"def","id":2,"ts":2,"type":"ops.y","payload":""}"#
            + "\n";
        let r = parse_global_events_body(0, &body);
        // Two valid rows; the malformed and the missing-
        // task_id rows are dropped.
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.next_cursor, 2);
    }

    #[test]
    fn parse_global_events_body_empty_returns_echoed_cursor() {
        let r = parse_global_events_body(42, "");
        assert!(r.items.is_empty());
        // Empty page must echo the caller's cursor unchanged
        // so polling clients don't reset.
        assert_eq!(r.next_cursor, 42);
    }

    #[test]
    fn parse_lineage_body_returns_honest_note_when_no_cross_task_edges() {
        let body = "root=abc\n\
                    tasks=abc\n\
                    cross_task_edges=0\n\
                    max_depth=4\n\
                    1\tretried_from\tabc\tabc\t1700000000\n";
        let r = parse_lineage_body("abc", body);
        assert_eq!(r.root_task_id, "abc");
        assert_eq!(r.tasks, vec!["abc"]);
        assert_eq!(r.cross_task_edge_count, 0);
        assert_eq!(r.max_depth_walked, 4);
        assert_eq!(r.edges.len(), 1);
        assert_eq!(r.edges[0].edge_type, "retried_from");
        assert_eq!(r.edges[0].related_task_id.as_deref(), Some("abc"));
        // Honest note — operators reading the JSON directly see
        // the gap surfaced.
        assert!(r.note.contains("reserved in the schema"));
    }

    #[test]
    fn parse_lineage_body_handles_dash_related_task() {
        // The coord emits `-` for a NULL related_task_id; the
        // parser must lift that to None rather than carry it
        // as a literal string.
        let body = "root=abc\n\
                    tasks=abc\n\
                    cross_task_edges=0\n\
                    max_depth=4\n\
                    5\tretried_from\tabc\t-\t1700000000\n";
        let r = parse_lineage_body("abc", body);
        assert_eq!(r.edges.len(), 1);
        assert!(r.edges[0].related_task_id.is_none());
    }

    #[test]
    fn parse_lineage_body_acknowledges_cross_task_count_in_note() {
        let body = "root=abc\n\
                    tasks=abc,xyz\n\
                    cross_task_edges=2\n\
                    max_depth=4\n";
        let r = parse_lineage_body("abc", body);
        assert!(r.note.contains("2 cross-task edge"));
    }

    #[test]
    fn parse_cursor_body_reads_investigation_marker_when_present() {
        // M63 wire format: 5th tab-delimited column is
        // investigation_marked_at, empty when unset.
        let body = "abc\trunning\tt0\t100\t1700000000\n\
                    def\tcompleted\tt1\t99\t\n\
                    next_cursor=\n";
        let (items, _next) = parse_cursor_body(body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].investigation_marked_at, Some(1700000000));
        assert_eq!(items[1].investigation_marked_at, None);
    }

    #[test]
    fn parse_cursor_body_empty_cursor_yields_none() {
        let body = "abc\trunning\tt0\t100\nnext_cursor=\n";
        let (items, next) = parse_cursor_body(body);
        assert_eq!(items.len(), 1);
        assert!(next.is_none());
    }

    #[test]
    fn parse_cursor_body_empty_page() {
        let body = "next_cursor=\n";
        let (items, next) = parse_cursor_body(body);
        assert!(items.is_empty());
        assert!(next.is_none());
    }

    #[test]
    fn parse_count_body_extracts_integer() {
        assert_eq!(parse_count_body("count=42\n"), Some(42));
        assert_eq!(parse_count_body("count=0"), Some(0));
        assert_eq!(parse_count_body(""), None);
        assert_eq!(parse_count_body("not a count"), None);
        // Extra lines don't break parsing.
        assert_eq!(parse_count_body("preamble\ncount=17\n"), Some(17));
    }

    #[test]
    fn parse_recover_body_handles_empty_scan() {
        let body = "recovered=0\n";
        let (ids, count) = parse_recover_body(body);
        assert!(ids.is_empty());
        assert_eq!(count, 0);
    }

    #[test]
    fn gateway_status_for_distinguishes_not_found() {
        assert_eq!(
            gateway_status_for("kind=5 cause=task.get: not found: abc"),
            StatusCode::NOT_FOUND,
        );
        assert_eq!(
            gateway_status_for("kind=1 cause=transport timeout"),
            StatusCode::BAD_GATEWAY,
        );
    }

    #[test]
    fn parse_edges_extracts_retried_from_with_all_fields() {
        // Real-shape line from the Coordinator: a retried_from
        // edge for attempt 2 (id 102) linking back to attempt 1
        // (id 101) via task.retry_requested event 47.
        let body = "5\tretried_from\t102\tabc\t101\t47\t1700000000\n";
        let edges = parse_edges(body);
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        assert_eq!(e.edge_id, 5);
        assert_eq!(e.edge_type, "retried_from");
        assert_eq!(e.attempt_id, Some(102));
        assert_eq!(e.related_task_id.as_deref(), Some("abc"));
        assert_eq!(e.related_attempt_id, Some(101));
        assert_eq!(e.spawned_by_event_id, Some(47));
        assert_eq!(e.created_at, 1_700_000_000);
    }

    #[test]
    fn parse_edges_handles_dash_placeholders() {
        // Coordinator emits `-` for any nullable column. Parser
        // must turn those into None without errors.
        let body = "9\tretried_from\t-\tabc\t-\t-\t1700000099\n";
        let edges = parse_edges(body);
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        assert!(e.attempt_id.is_none());
        assert!(e.related_attempt_id.is_none());
        assert!(e.spawned_by_event_id.is_none());
        assert_eq!(e.related_task_id.as_deref(), Some("abc"));
    }

    #[test]
    fn parse_edges_skips_malformed_lines_silently() {
        let body = "not-a-real-line\n5\tretried_from\t1\ta\t2\t3\t100\nbroken\n";
        let edges = parse_edges(body);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].edge_id, 5);
    }

    #[test]
    fn parse_edges_empty_body_returns_empty_vec() {
        assert!(parse_edges("").is_empty());
        assert!(parse_edges("\n\n").is_empty());
    }

    #[test]
    fn parse_retry_body_extracts_accepted_outcome() {
        let r = parse_retry_body("accepted attempt=2 of_budget=3\n");
        assert_eq!(r.outcome, "accepted");
        assert_eq!(r.attempt, Some(2));
        assert_eq!(r.of_budget, Some(3));
        assert!(r.last_failure_class.is_none());
    }

    #[test]
    fn parse_retry_body_extracts_exhausted_outcome() {
        let r = parse_retry_body("exhausted retry_count=3 budget=3\n");
        assert_eq!(r.outcome, "exhausted");
        assert_eq!(r.attempt, Some(3));
        assert_eq!(r.of_budget, Some(3));
    }

    #[test]
    fn parse_retry_body_falls_back_to_unknown() {
        // Future-proof: if the Coordinator adds a new outcome
        // shape, the parser surfaces it as "unknown" with the
        // raw detail so the dashboard can still show something.
        let r = parse_retry_body("some unexpected line");
        assert_eq!(r.outcome, "unknown");
        assert_eq!(r.detail, "some unexpected line");
    }

    #[test]
    fn parse_failure_class_finds_the_marker() {
        let body = "task_id=abc\nstatus=failed\nlast_failure_class=transient\nevents=[]\n";
        assert_eq!(
            parse_failure_class_from_body(body).as_deref(),
            Some("transient")
        );
        // Returns None when absent.
        let body2 = "task_id=abc\nstatus=completed\nevents=[]\n";
        assert!(parse_failure_class_from_body(body2).is_none());
    }

    #[test]
    fn compact_query_default_round_trips_absent_param() {
        // CompactQuery uses #[serde(default)] for max_age_secs
        // so an absent ?max_age_secs= parses to None — the
        // handler then returns a clear 400 instead of a 500.
        // Round-trip via JSON proves the default behaviour
        // (axum uses serde under the hood for query parsing).
        let q: CompactQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(q.max_age_secs, None);
        let q: CompactQuery = serde_json::from_str(r#"{"max_age_secs":2592000}"#).unwrap();
        assert_eq!(q.max_age_secs, Some(2_592_000));
        // Negative integers deserialise fine here; the
        // handler is responsible for rejecting them (and
        // does — see relix-runtime coordinator tests).
        let q: CompactQuery = serde_json::from_str(r#"{"max_age_secs":-1}"#).unwrap();
        assert_eq!(q.max_age_secs, Some(-1));
    }

    #[test]
    fn run_stream_event_name_maps_execution_types_and_ignores_others() {
        // Each execution chronicle type maps to its normalized SSE name.
        assert_eq!(
            run_stream_event_name("brief.run_started"),
            Some("run_started")
        );
        assert_eq!(
            run_stream_event_name("brief.shift_done"),
            Some("run_finished")
        );
        assert_eq!(
            run_stream_event_name("brief.dispatch_failed"),
            Some("run_finished")
        );
        assert_eq!(
            run_stream_event_name("brief.continued"),
            Some("run_finished")
        );
        assert_eq!(
            run_stream_event_name("brief.run_recovered"),
            Some("run_finished")
        );
        assert_eq!(
            run_stream_event_name("brief.run_refused"),
            Some("run_finished")
        );
        assert_eq!(
            run_stream_event_name("brief.run_cancel_requested"),
            Some("run_cancel_requested")
        );
        assert_eq!(
            run_stream_event_name("brief.board_moved"),
            Some("brief_moved")
        );
        assert_eq!(
            run_stream_event_name("brief.run_reviewed"),
            Some("review_changed")
        );
        assert_eq!(
            run_stream_event_name("brief.run_applied"),
            Some("apply_changed")
        );
        assert_eq!(
            run_stream_event_name("brief.run_discarded"),
            Some("apply_changed")
        );
        // Non-execution chronicle types are not forwarded on this stream.
        assert_eq!(run_stream_event_name("brief.comment"), None);
        assert_eq!(run_stream_event_name("brief.created"), None);
        assert_eq!(run_stream_event_name("task.attempt_started"), None);
    }
}
