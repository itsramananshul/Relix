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

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::config::AppState;

/// Compact task line returned by `GET /v1/tasks`.
#[derive(Debug, Serialize)]
pub struct TaskListEntry {
    pub task_id: String,
    pub status: String,
    pub title: String,
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
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let body = rec
        .recover()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(ApiError { error: e })))?;
    let (ids, _count) = parse_recover_body(&body);
    let count = ids.len();
    Ok(Json(RecoverResponse {
        recovered: ids,
        count,
    }))
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

/// One-call full reconstruction: task detail + attempts + summary.
/// Returns the same shapes the per-resource endpoints do, packed
/// into one round-trip so dashboard initial-render doesn't need
/// three separate fetches.
#[derive(Debug, Serialize)]
pub struct TaskLineage {
    pub task: TaskDetail,
    pub summary: TaskSummary,
    pub attempts: Vec<TaskAttempt>,
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
    Ok(Json(TaskLineage {
        task,
        summary,
        attempts,
    }))
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
/// `task_id\tstatus\ttitle\tupdated_at` rows followed by a
/// trailing `next_cursor=<value>\n`. Returns the rows + the
/// optional cursor (None on empty value).
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
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() < 3 {
            continue;
        }
        items.push(TaskListEntry {
            task_id: parts[0].to_string(),
            status: parts[1].to_string(),
            title: parts[2].to_string(),
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
        assert_eq!(
            pj.get("attempt_num").and_then(|v| v.as_i64()),
            Some(1)
        );
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
        assert_eq!(next.as_deref(), Some("99:def"));
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
}
