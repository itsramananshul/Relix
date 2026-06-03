//! RELIX-7.24 — HTTP proxies for the `planning.*` capability
//! surface.
//!
//! Five endpoints, each a thin forwarder to a `planning.*`
//! capability on the coordinator:
//!
//! - `POST /v1/planning/plan` — `planning.create_plan`
//! - `GET  /v1/planning/agents` — `planning.list_agents`
//! - `POST /v1/planning/agents/search` — `planning.find_agents`
//! - `POST /v1/planning/validate` — `planning.validate_spec`
//! - `GET  /v1/planning/status` — `planning.orchestrator_status`
//!   (Stage-1/3: wired view of `[planning]` + dispatcher liveness)
//!
//! Error mapping mirrors the knowledge / confidence / metrics
//! endpoints:
//! - `INVALID_ARGS` from the responder → `400 Bad Request`
//! - peer alias missing → `404 Not Found`
//! - responder fault → `502 Bad Gateway`
//! - bridge mesh client not ready → `503 Service Unavailable`

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject, current_tenant};

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PeerQuery {
    /// Override the coordinator peer alias. Default
    /// `"coordinator"`.
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePlanRequest {
    pub spec: String,
    #[serde(default)]
    pub max_agents: Option<usize>,
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchAgentsRequest {
    pub task: String,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ValidateSpecRequest {
    pub spec: String,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DecidePlanRequest {
    pub plan_id: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListApprovalsQuery {
    /// Optional `pending|approved|rejected|expired` filter.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ExportSpecQuery {
    /// `"json"` (default) | `"markdown"`.
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `POST /v1/planning/plan`
pub async fn create_plan(
    State(state): State<AppState>,
    Json(req): Json<CreatePlanRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.spec.trim().is_empty() {
        return bad_request("spec is required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = create_plan_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("spec".into(), Value::from(req.spec));
    if let Some(n) = req.max_agents {
        body.insert("max_agents".into(), Value::from(n));
    }
    if let Some(d) = req.dry_run {
        body.insert("dry_run".into(), Value::from(d));
    }
    match call_peer_json(
        &state,
        &peer,
        "planning.create_plan",
        &Value::Object(body),
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_planning_activity(
                &state,
                &peer,
                "planning.create_plan",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_planning_activity(
                &state,
                &peer,
                "planning.create_plan",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

/// `GET /v1/planning/agents`
pub async fn list_agents(
    State(state): State<AppState>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "planning.list_agents", &Value::Null, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/planning/agents/search`
pub async fn search_agents(
    State(state): State<AppState>,
    Json(req): Json<SearchAgentsRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.task.trim().is_empty() {
        return bad_request("task is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "task": req.task });
    match call_peer_json(&state, &peer, "planning.find_agents", &body, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/planning/validate`
pub async fn validate_spec(
    State(state): State<AppState>,
    Json(req): Json<ValidateSpecRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.spec.trim().is_empty() {
        return bad_request("spec is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "spec": req.spec });
    match call_peer_json(&state, &peer, "planning.validate_spec", &body, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/planning/status`
///
/// RELIX-7.24 Stage-1/3: thin proxy onto
/// `planning.orchestrator_status`. Returns the wired
/// `[planning]` block plus a `dispatcher_live` flag.
pub async fn orchestrator_status(
    State(state): State<AppState>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(
        &state,
        &peer,
        "planning.orchestrator_status",
        &Value::Null,
        None,
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/planning/approve` — RELIX-7.24 Stage-4.
pub async fn approve_plan(
    State(state): State<AppState>,
    Json(req): Json<DecidePlanRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.plan_id.trim().is_empty() {
        return bad_request("plan_id is required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = decide_plan_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({
        "plan_id": req.plan_id,
        "note": req.note,
    });
    match call_peer_json(
        &state,
        &peer,
        "planning.approve_plan",
        &body,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_planning_activity(
                &state,
                &peer,
                "planning.approve_plan",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_planning_activity(
                &state,
                &peer,
                "planning.approve_plan",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

/// `POST /v1/planning/reject` — RELIX-7.24 Stage-4.
pub async fn reject_plan(
    State(state): State<AppState>,
    Json(req): Json<DecidePlanRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.plan_id.trim().is_empty() {
        return bad_request("plan_id is required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = decide_plan_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({
        "plan_id": req.plan_id,
        "note": req.note,
    });
    match call_peer_json(
        &state,
        &peer,
        "planning.reject_plan",
        &body,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_planning_activity(
                &state,
                &peer,
                "planning.reject_plan",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_planning_activity(
                &state,
                &peer,
                "planning.reject_plan",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

/// `GET /v1/planning/approvals` — RELIX-7.24 Stage-4.
pub async fn list_approvals(
    State(state): State<AppState>,
    Query(q): Query<ListApprovalsQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(s) = q.status.as_ref() {
        body.insert("status".into(), Value::from(s.clone()));
    }
    match call_peer_json(
        &state,
        &peer,
        "planning.list_approvals",
        &Value::Object(body),
        None,
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/planning/approvals/:id` — RELIX-7.24 Stage-4.
pub async fn get_approval(
    State(state): State<AppState>,
    axum::extract::Path(plan_id): axum::extract::Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if plan_id.trim().is_empty() {
        return bad_request("plan_id is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "plan_id": plan_id });
    match call_peer_json(&state, &peer, "planning.get_approval", &body, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/planning/verification/:id` — RELIX-7.24 Stage-5.
pub async fn verification_log(
    State(state): State<AppState>,
    axum::extract::Path(plan_id): axum::extract::Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if plan_id.trim().is_empty() {
        return bad_request("plan_id is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "plan_id": plan_id });
    match call_peer_json(&state, &peer, "planning.verification_log", &body, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/planning/export/:id` — RELIX-7.24 follow-up. Thin
/// proxy onto `planning.export_spec`. Default format is JSON;
/// pass `?format=markdown` to get the human-readable summary.
pub async fn export_spec(
    State(state): State<AppState>,
    axum::extract::Path(plan_id): axum::extract::Path<String>,
    Query(q): Query<ExportSpecQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if plan_id.trim().is_empty() {
        return bad_request("plan_id is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let format = q
        .format
        .clone()
        .unwrap_or_else(|| "json".to_string())
        .trim()
        .to_ascii_lowercase();
    let body = serde_json::json!({
        "plan_id": plan_id,
        "format": format,
    });
    match call_peer_json(&state, &peer, "planning.export_spec", &body, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/planning/verification/:id/stream` — RELIX-7.24
/// Stage-5 streaming variant. Opens a Server-Sent Events
/// connection that polls `planning.verification_log` on the
/// coordinator every 500ms and emits each NEW entry as one
/// SSE `event: entry` payload. Emits `event: heartbeat`
/// every 10s to keep proxies + load balancers from closing
/// the connection on idle plans. Closes either when the
/// consumer disconnects OR after a hard 10-minute cap (the
/// consumer can reconnect to keep watching).
///
/// Event shapes:
///
/// - `event: entry` + `data: {VerificationEntry JSON}`
/// - `event: heartbeat` + `data: {"seen":<N>}` every 10s of idle
/// - `event: done` + `data: {"seen":<N>, "reason":"<why>"}`
///   when the cap fires or polling encounters a fatal error
pub async fn verification_stream(
    State(state): State<AppState>,
    axum::extract::Path(plan_id): axum::extract::Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use std::convert::Infallible;

    if plan_id.trim().is_empty() {
        return bad_request("plan_id is required");
    }

    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let state = state.clone();
    let plan_id = plan_id.clone();

    let stream = async_stream::stream! {
        let poll_interval = std::time::Duration::from_millis(500);
        let hard_cap = std::time::Duration::from_secs(600);
        let heartbeat_after = std::time::Duration::from_secs(10);
        let started = std::time::Instant::now();
        let mut seen: usize = 0;
        let mut last_emit = std::time::Instant::now();
        let body = serde_json::json!({ "plan_id": plan_id });
        loop {
            if started.elapsed() >= hard_cap {
                let payload = serde_json::json!({
                    "seen": seen,
                    "reason": "10-minute stream cap reached; reconnect to keep watching",
                });
                yield Ok::<_, Infallible>(
                    Event::default()
                        .event("done")
                        .data(payload.to_string()),
                );
                break;
            }
            match call_peer_json(&state, &peer, "planning.verification_log", &body, None).await {
                Ok(v) => {
                    let entries = v
                        .get("entries")
                        .and_then(serde_json::Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let total = entries.len();
                    if total > seen {
                        for entry in &entries[seen..] {
                            yield Ok(Event::default()
                                .event("entry")
                                .data(entry.to_string()));
                        }
                        seen = total;
                        last_emit = std::time::Instant::now();
                    } else if last_emit.elapsed() >= heartbeat_after {
                        yield Ok(Event::default()
                            .event("heartbeat")
                            .data(serde_json::json!({"seen": seen}).to_string()));
                        last_emit = std::time::Instant::now();
                    }
                }
                Err(_) => {
                    // Coordinator unreachable. Emit a single
                    // done event and terminate so the consumer
                    // doesn't sit on a half-broken stream.
                    let payload = serde_json::json!({
                        "seen": seen,
                        "reason": "coordinator verification_log call failed; stream closed",
                    });
                    yield Ok(Event::default()
                        .event("done")
                        .data(payload.to_string()));
                    break;
                }
            }
            tokio::time::sleep(poll_interval).await;
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new())
        .into_response()
}

// ── shared helpers ────────────────────────────────────────

fn bad_request(msg: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

/// Clean "feature not enabled" body (HTTP 200) when the responder
/// reports UNKNOWN_METHOD (planning orchestrator not enabled), so the
/// panel renders an empty state instead of a 502.
fn unavailable(method: &str) -> Value {
    serde_json::json!({
        "available": false,
        "reason": format!("capability '{method}' is not enabled on this deployment"),
    })
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn clean_optional_id(value: Option<&str>, field: &str) -> Result<Option<String>, String> {
    let Some(clean) = clean_optional(value) else {
        return Ok(None);
    };
    if clean.len() == 32 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(clean))
    } else {
        Err(format!("{field} must be 32 hex chars"))
    }
}

fn attach_scope(value: &mut Value, task_id: Option<&str>, run_id: Option<&str>) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if let Some(task_id) = task_id {
        obj.insert("task_id".into(), Value::String(task_id.to_string()));
    }
    if let Some(run_id) = run_id {
        obj.insert("run_id".into(), Value::String(run_id.to_string()));
    }
}

fn create_plan_detail(req: &CreatePlanRequest) -> String {
    format!(
        "spec_len={}; max_agents={}; dry_run={}",
        req.spec.len(),
        req.max_agents
            .map(|n| n.to_string())
            .unwrap_or_else(|| "none".into()),
        req.dry_run
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".into())
    )
}

fn decide_plan_detail(req: &DecidePlanRequest) -> String {
    format!(
        "plan_id={}; note_len={}",
        req.plan_id.trim(),
        req.note.as_deref().map(str::len).unwrap_or(0)
    )
}

fn record_planning_activity(
    state: &AppState,
    peer: &str,
    method: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "planning".into());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer,
            method,
            task_id,
            run_id,
            decision,
            detail,
        },
    ) {
        tracing::warn!(error = %e, method, "failed to append planning activity");
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), task_id) {
        let payload = format!("peer={peer} outcome={decision} {detail}");
        let rec = rec.clone();
        let task_id = task_id.to_string();
        let event_type = method.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, &event_type, &payload).await;
        });
    }
}

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
    task_id: Option<&str>,
) -> Result<Value, axum::response::Response> {
    use axum::response::IntoResponse;
    let mesh = match state.mesh_client.as_ref() {
        Some(m) => m,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError {
                    error: "bridge mesh client not initialized".into(),
                }),
            )
                .into_response());
        }
    };
    let arg_bytes = match serde_json::to_vec(args) {
        Ok(b) => b,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: format!("encode args: {e}"),
                }),
            )
                .into_response());
        }
    };
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 300);
    let envelope = build_request_with_tenant(
        method,
        arg_bytes,
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
        (status, Json(ApiError { error: msg })).into_response()
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
            .into_response()
    })?;
    match resp.res {
        ResponseResult::Ok(body) => {
            let text = String::from_utf8(body.to_vec()).map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: format!("response body utf8: {e}"),
                    }),
                )
                    .into_response()
            })?;
            serde_json::from_str::<Value>(&text).map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: format!("response body not JSON: {e} (body={text:?})"),
                    }),
                )
                    .into_response()
            })
        }
        ResponseResult::Err(env) => {
            if env.kind == relix_core::types::error_kinds::UNKNOWN_METHOD {
                return Ok(unavailable(method));
            }
            let status = if env.kind == relix_core::types::error_kinds::INVALID_ARGS {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={}", env.kind, env.cause),
                }),
            )
                .into_response())
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from coordinator".into(),
            }),
        )
            .into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_request_returns_400_with_error_body() {
        let resp = bad_request("missing arg");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_plan_request_decodes_minimal_body() {
        let r: CreatePlanRequest = serde_json::from_str(r#"{"spec":"do X"}"#).expect("parse");
        assert_eq!(r.spec, "do X");
        assert!(r.max_agents.is_none());
        assert!(r.dry_run.is_none());
    }

    #[test]
    fn create_plan_request_decodes_full_body() {
        let r: CreatePlanRequest = serde_json::from_str(
            r#"{
                "spec":"do X",
                "max_agents":5,
                "dry_run":true,
                "peer":"alt",
                "task_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id":"run-1"
            }"#,
        )
        .expect("parse");
        assert_eq!(r.max_agents, Some(5));
        assert_eq!(r.dry_run, Some(true));
        assert_eq!(r.peer.as_deref(), Some("alt"));
        assert_eq!(
            r.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(r.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn planning_detail_does_not_copy_spec_or_operator_note() {
        let create = CreatePlanRequest {
            spec: "secret product launch plan".into(),
            max_agents: Some(3),
            dry_run: Some(false),
            peer: None,
            task_id: None,
            run_id: None,
        };
        let detail = create_plan_detail(&create);
        assert!(detail.contains("spec_len=26"));
        assert!(!detail.contains("secret product launch plan"));

        let decide = DecidePlanRequest {
            plan_id: "plan-1".into(),
            note: Some("do not log this operator note".into()),
            peer: None,
            task_id: None,
            run_id: None,
        };
        let detail = decide_plan_detail(&decide);
        assert!(detail.contains("plan_id=plan-1"));
        assert!(detail.contains("note_len=29"));
        assert!(!detail.contains("do not log this operator note"));
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut value = serde_json::json!({ "plan_id": "plan-1" });
        attach_scope(
            &mut value,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some("run-1"),
        );
        assert_eq!(value["task_id"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(value["run_id"], "run-1");

        let mut scalar = serde_json::json!("ok");
        attach_scope(&mut scalar, Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), None);
        assert_eq!(scalar, serde_json::json!("ok"));
    }

    #[test]
    fn clean_optional_id_rejects_invalid_task_id() {
        assert_eq!(
            clean_optional_id(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "task_id")
                .unwrap()
                .as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(clean_optional_id(Some("bad"), "task_id").is_err());
        assert_eq!(clean_optional_id(Some(" "), "task_id").unwrap(), None);
    }
}
