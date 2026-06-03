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

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject};

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
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SpawnResponse {
    pub child_task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
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
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
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
    let parent_task_id = clean_task_id(&parent, "parent_task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let goal = require_field(&req.goal, "goal")?;
    let context = req.context.unwrap_or_default();
    let target_subject_id = req.target_subject_id.unwrap_or_default();
    let depth = req.depth.unwrap_or(0);
    let detail = spawn_detail(&parent_task_id, goal.len(), context.len(), depth);
    // Reject `|` in any field — they'd break the wire format.
    for (name, val) in [
        ("parent_task_id", parent_task_id.as_str()),
        ("goal", goal.as_str()),
        ("target_subject_id", target_subject_id.as_str()),
    ] {
        if val.contains('|') {
            return Err(bad(format!("{name} must not contain `|`")));
        }
    }
    let arg = format!("{parent_task_id}|{goal}|{context}|{target_subject_id}|{depth}");
    let body = match call_peer_string(
        &state,
        DEFAULT_PEER,
        "delegate.spawn",
        arg.as_bytes(),
        Some(&parent_task_id),
    )
    .await
    {
        Ok(body) => {
            record_delegate_activity(
                &state,
                DelegateActivity {
                    task_id: Some(&parent_task_id),
                    run_id: run_id.as_deref(),
                    method: "delegate.spawn",
                    decision: "ok",
                    detail: &detail,
                },
            );
            body
        }
        Err(err) => {
            record_delegate_activity(
                &state,
                DelegateActivity {
                    task_id: Some(&parent_task_id),
                    run_id: run_id.as_deref(),
                    method: "delegate.spawn",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    };
    Ok(Json(SpawnResponse {
        child_task_id: body.trim().to_string(),
        parent_task_id: Some(parent_task_id),
        run_id,
    }))
}

pub async fn result(
    State(state): State<AppState>,
    Path(child_id): Path<String>,
) -> Result<Json<ResultResponse>, (StatusCode, Json<ApiError>)> {
    let body = call_peer_string(
        &state,
        DEFAULT_PEER,
        "delegate.result",
        child_id.as_bytes(),
        None,
    )
    .await?;
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
    let child_task_id = clean_task_id(&child_id, "child_task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let reason = req.reason.unwrap_or_default();
    if child_task_id.contains('|') {
        return Err(bad("child_task_id must not contain `|`".into()));
    }
    let detail = cancel_detail(&child_task_id, reason.len());
    let arg = format!("{child_task_id}|{reason}");
    match call_peer_string(
        &state,
        DEFAULT_PEER,
        "delegate.cancel",
        arg.as_bytes(),
        Some(&child_task_id),
    )
    .await
    {
        Ok(_) => {
            record_delegate_activity(
                &state,
                DelegateActivity {
                    task_id: Some(&child_task_id),
                    run_id: run_id.as_deref(),
                    method: "delegate.cancel",
                    decision: "ok",
                    detail: &detail,
                },
            );
            Ok(Json(OkResponse {
                ok: true,
                task_id: Some(child_task_id),
                run_id,
            }))
        }
        Err(err) => {
            record_delegate_activity(
                &state,
                DelegateActivity {
                    task_id: Some(&child_task_id),
                    run_id: run_id.as_deref(),
                    method: "delegate.cancel",
                    decision: "err",
                    detail: &detail,
                },
            );
            Err(err)
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    Path(parent_id): Path<String>,
) -> Result<Json<ListResponse>, (StatusCode, Json<ApiError>)> {
    let body = call_peer_string(
        &state,
        DEFAULT_PEER,
        "delegate.list",
        parent_id.as_bytes(),
        None,
    )
    .await?;
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

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn clean_task_id(value: &str, field: &str) -> Result<String, (StatusCode, Json<ApiError>)> {
    let clean = value.trim();
    if clean.len() == 32 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(clean.to_string())
    } else {
        Err(bad(format!("{field} must be 32 hex chars")))
    }
}

fn spawn_detail(parent_task_id: &str, goal_len: usize, context_len: usize, depth: usize) -> String {
    format!(
        "method=delegate.spawn; parent_task_id={parent_task_id}; goal_len={goal_len}; context_len={context_len}; depth={depth}"
    )
}

fn cancel_detail(child_task_id: &str, reason_len: usize) -> String {
    format!("method=delegate.cancel; child_task_id={child_task_id}; reason_len={reason_len}")
}

struct DelegateActivity<'a> {
    task_id: Option<&'a str>,
    run_id: Option<&'a str>,
    method: &'a str,
    decision: &'a str,
    detail: &'a str,
}

fn record_delegate_activity(state: &AppState, activity: DelegateActivity<'_>) {
    let tenant_id = crate::tenant::current_tenant_or_none()
        .as_deref()
        .unwrap_or(DEFAULT_TENANT)
        .to_string();
    let actor = current_subject().unwrap_or_else(|| activity.method.to_string());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer: DEFAULT_PEER,
            method: activity.method,
            task_id: activity.task_id,
            run_id: activity.run_id,
            decision: activity.decision,
            detail: activity.detail,
        },
    ) {
        tracing::warn!(
            error = %e,
            method = activity.method,
            "failed to append delegate activity"
        );
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), activity.task_id) {
        let payload = format!(
            "peer={DEFAULT_PEER} outcome={} {}",
            activity.decision, activity.detail
        );
        let rec = rec.clone();
        let task_id = task_id.to_string();
        let event_type = activity.method.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, &event_type, &payload).await;
        });
    }
}

async fn call_peer_string(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
    task_id: Option<&str>,
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
        task_id.map(str::to_string),
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

    #[test]
    fn spawn_request_accepts_run_context() {
        let req: SpawnRequest = serde_json::from_value(serde_json::json!({
            "parent_task_id": "0123456789abcdef0123456789abcdef",
            "goal": "summarize",
            "context": "private context",
            "depth": 1,
            "run_id": "run-1"
        }))
        .unwrap();
        assert_eq!(req.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn clean_task_id_accepts_only_32_hex() {
        assert_eq!(
            clean_task_id(" 0123456789abcdef0123456789abcdef ", "task_id")
                .unwrap()
                .as_str(),
            "0123456789abcdef0123456789abcdef"
        );
        let err = clean_task_id("abc", "task_id").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
    }

    #[test]
    fn spawn_detail_does_not_copy_goal_or_context() {
        let goal = "write the sensitive plan";
        let context = "private operator context";
        let detail = spawn_detail(
            "0123456789abcdef0123456789abcdef",
            goal.len(),
            context.len(),
            2,
        );
        assert!(detail.contains("parent_task_id=0123456789abcdef0123456789abcdef"));
        assert!(detail.contains("goal_len=24"));
        assert!(detail.contains("context_len=24"));
        assert!(detail.contains("depth=2"));
        assert!(!detail.contains(goal));
        assert!(!detail.contains(context));
    }

    #[test]
    fn cancel_detail_does_not_copy_reason() {
        let reason = "operator included sensitive reason";
        let detail = cancel_detail("0123456789abcdef0123456789abcdef", reason.len());
        assert!(detail.contains("child_task_id=0123456789abcdef0123456789abcdef"));
        assert!(detail.contains("reason_len=34"));
        assert!(!detail.contains(reason));
    }
}
