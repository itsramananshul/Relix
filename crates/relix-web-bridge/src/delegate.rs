//! HTTP proxies for the delegation surface.
//!
//! Four endpoints, each proxying one `delegate.*` capability
//! on the coordinator and reshaping the pipe/tab-delimited
//! wire body into typed JSON for the dashboard + CLI.
//!
//! - `POST /v1/delegate/spawn`            { parent_task_id, goal, context?, target_subject_id?, depth? }
//! - `GET  /v1/delegate/result/:child_id`
//! - `POST /v1/delegate/cancel/:child_id` { reason? }
//! - `GET  /v1/delegate/list/:parent_id`

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct SpawnRequest {
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub target_subject_id: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SpawnResponse {
    pub child_task_id: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ResultResponse {
    pub status: String,
    pub result_preview: String,
    pub completed_at: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CancelRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DelegationRow {
    pub child_task_id: String,
    pub goal_preview: String,
    pub status: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ListResponse {
    pub delegations: Vec<DelegationRow>,
}

pub async fn spawn(
    State(state): State<AppState>,
    Json(req): Json<SpawnRequest>,
) -> Result<Json<SpawnResponse>, (StatusCode, Json<ApiError>)> {
    let parent = require_field(&req.parent_task_id, "parent_task_id")?;
    let goal = require_field(&req.goal, "goal")?;
    let context = req.context.unwrap_or_default();
    let target_subject_id = req.target_subject_id.unwrap_or_default();
    let depth = req.depth.unwrap_or(0);
    // Reject `|` in any field — they'd break the wire format.
    for (name, val) in [
        ("parent_task_id", parent.as_str()),
        ("goal", goal.as_str()),
        ("target_subject_id", target_subject_id.as_str()),
    ] {
        if val.contains('|') {
            return Err(bad(format!("{name} must not contain `|`")));
        }
    }
    let arg = format!("{parent}|{goal}|{context}|{target_subject_id}|{depth}");
    let body = call_peer_string(&state, DEFAULT_PEER, "delegate.spawn", arg.as_bytes()).await?;
    Ok(Json(SpawnResponse {
        child_task_id: body.trim().to_string(),
    }))
}

pub async fn result(
    State(state): State<AppState>,
    Path(child_id): Path<String>,
) -> Result<Json<ResultResponse>, (StatusCode, Json<ApiError>)> {
    let body =
        call_peer_string(&state, DEFAULT_PEER, "delegate.result", child_id.as_bytes()).await?;
    let parsed = parse_result_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!("delegate.result returned an unparseable body: {body:?}"),
        }),
    ))?;
    Ok(Json(parsed))
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(child_id): Path<String>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let reason = req.reason.unwrap_or_default();
    if child_id.contains('|') {
        return Err(bad("child_task_id must not contain `|`".into()));
    }
    let arg = format!("{child_id}|{reason}");
    let _ = call_peer_string(&state, DEFAULT_PEER, "delegate.cancel", arg.as_bytes()).await?;
    Ok(Json(OkResponse { ok: true }))
}

pub async fn list(
    State(state): State<AppState>,
    Path(parent_id): Path<String>,
) -> Result<Json<ListResponse>, (StatusCode, Json<ApiError>)> {
    let body =
        call_peer_string(&state, DEFAULT_PEER, "delegate.list", parent_id.as_bytes()).await?;
    let delegations = parse_list_body(&body);
    Ok(Json(ListResponse { delegations }))
}

// ── Parsers ──────────────────────────────────────────────

pub fn parse_result_body(body: &str) -> Option<ResultResponse> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `status|preview|completed_at`
    let parts: Vec<&str> = trimmed.splitn(3, '|').collect();
    if parts.len() != 3 {
        return None;
    }
    let completed_raw: i64 = parts[2].parse().ok()?;
    Some(ResultResponse {
        status: parts[0].to_string(),
        result_preview: parts[1].to_string(),
        completed_at: if completed_raw < 0 {
            None
        } else {
            Some(completed_raw)
        },
    })
}

pub fn parse_list_body(body: &str) -> Vec<DelegationRow> {
    body.lines()
        .filter(|line| !line.starts_with("count=") && !line.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.splitn(4, '\t').collect();
            if cols.len() != 4 {
                return None;
            }
            Some(DelegationRow {
                child_task_id: cols[0].into(),
                goal_preview: cols[1].into(),
                status: cols[2].into(),
                created_at: cols[3].parse().ok()?,
            })
        })
        .collect()
}

fn require_field(v: &Option<String>, name: &str) -> Result<String, (StatusCode, Json<ApiError>)> {
    let s = v.as_deref().unwrap_or("").trim();
    if s.is_empty() {
        return Err(bad(format!("{name} is required")));
    }
    Ok(s.to_string())
}

fn bad(msg: String) -> (StatusCode, Json<ApiError>) {
    (StatusCode::BAD_REQUEST, Json(ApiError { error: msg }))
}

async fn call_peer_string(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized".into(),
        }),
    ))?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 60);
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = mesh.call(alias, envelope).await.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_ascii_lowercase();
        let status = if lower.contains("unknown alias") || lower.contains("no peer") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_GATEWAY
        };
        (status, Json(ApiError { error: msg }))
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
    })?;
    match resp.res {
        ResponseResult::Ok(body) => String::from_utf8(body.to_vec()).map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: format!("response body utf8: {e}"),
                }),
            )
        }),
        ResponseResult::Err(env) => {
            let cause = env.cause;
            let lower = cause.to_ascii_lowercase();
            let status = if lower.contains("not found") {
                StatusCode::NOT_FOUND
            } else if env.kind == 5 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={cause}", env.kind),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from coordinator".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_result_pending_no_preview_no_timestamp() {
        let body = "pending||-1\n";
        let r = parse_result_body(body).unwrap();
        assert_eq!(r.status, "pending");
        assert_eq!(r.result_preview, "");
        assert_eq!(r.completed_at, None);
    }

    #[test]
    fn parse_result_completed_with_preview_and_timestamp() {
        let body = "completed|the answer|1700000000\n";
        let r = parse_result_body(body).unwrap();
        assert_eq!(r.status, "completed");
        assert_eq!(r.result_preview, "the answer");
        assert_eq!(r.completed_at, Some(1_700_000_000));
    }

    #[test]
    fn parse_result_empty_body_returns_none() {
        assert!(parse_result_body("").is_none());
    }

    #[test]
    fn parse_result_malformed_field_count_returns_none() {
        assert!(parse_result_body("only|two\n").is_none());
    }

    #[test]
    fn parse_list_typical_two_row_body_plus_count_line() {
        let body = "abc\tdo the thing\tcompleted\t100\nxyz\tanother goal\tpending\t200\ncount=2\n";
        let v = parse_list_body(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].child_task_id, "abc");
        assert_eq!(v[0].goal_preview, "do the thing");
        assert_eq!(v[0].status, "completed");
        assert_eq!(v[0].created_at, 100);
        assert_eq!(v[1].child_task_id, "xyz");
    }

    #[test]
    fn parse_list_empty_returns_empty_vec() {
        assert!(parse_list_body("").is_empty());
        assert!(parse_list_body("count=0\n").is_empty());
    }
}
