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
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<TaskListEntry>>, (StatusCode, Json<ApiError>)> {
    let Some(rec) = state.task_recorder.as_ref() else {
        return Err(no_coordinator());
    };
    let limit = q.limit.unwrap_or(50);
    let body = rec
        .list(limit)
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
        if let Some(s) = &q.status
            && !s.is_empty()
            && parts[1] != s
        {
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

/// Parse the Coordinator's hand-built JSON event array. Same shape
/// as the CLI's parser (`relix_cli::task::parse_events_array`), but
/// duplicated here so the bridge stays independent of the CLI
/// crate.
fn parse_events_array(s: &str) -> Vec<TaskEvent> {
    let s = s.trim();
    let Some(inner) = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')) else {
        return Vec::new();
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut buf = String::new();
    let mut in_str = false;
    let mut esc = false;
    for c in inner.chars() {
        if in_str {
            buf.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '{' => {
                depth += 1;
                buf.push(c);
            }
            '}' => {
                depth -= 1;
                buf.push(c);
                if depth == 0 {
                    if let Some(obj) = parse_event_object(buf.trim()) {
                        out.push(obj);
                    }
                    buf.clear();
                }
            }
            ',' if depth == 0 => {}
            '"' => {
                in_str = true;
                buf.push(c);
            }
            _ => buf.push(c),
        }
    }
    out
}

fn parse_event_object(obj: &str) -> Option<TaskEvent> {
    let body = obj.strip_prefix('{')?.strip_suffix('}')?;
    let mut id: Option<i64> = None;
    let mut ts: Option<i64> = None;
    let mut ev_type: Option<String> = None;
    let mut payload: Option<String> = None;
    let mut chars = body.chars().peekable();
    while chars.peek().is_some() {
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        if chars.next() != Some('"') {
            return None;
        }
        let mut key = String::new();
        for c in chars.by_ref() {
            if c == '"' {
                break;
            }
            key.push(c);
        }
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ':') {
            chars.next();
        }
        match chars.peek() {
            Some('"') => {
                chars.next();
                let mut v = String::new();
                let mut esc = false;
                for c in chars.by_ref() {
                    if esc {
                        match c {
                            'n' => v.push('\n'),
                            'r' => v.push('\r'),
                            't' => v.push('\t'),
                            '"' => v.push('"'),
                            '\\' => v.push('\\'),
                            other => v.push(other),
                        }
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '"' {
                        break;
                    } else {
                        v.push(c);
                    }
                }
                match key.as_str() {
                    "type" => ev_type = Some(v),
                    "payload" => payload = Some(v),
                    _ => {}
                }
            }
            Some(_) => {
                let mut v = String::new();
                while let Some(c) = chars.peek() {
                    if c.is_ascii_digit() || *c == '-' {
                        v.push(*c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match key.as_str() {
                    "id" => id = v.parse().ok(),
                    "ts" => ts = v.parse().ok(),
                    _ => {}
                }
            }
            None => break,
        }
    }
    Some(TaskEvent {
        event_id: id?,
        ts: ts?,
        event_type: ev_type?,
        payload: payload.unwrap_or_default(),
    })
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
