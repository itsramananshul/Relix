//! RELIX-7.16 — HTTP proxies for the agent-to-agent knowledge
//! transfer surface.
//!
//! Five endpoints, all forwarders to the `knowledge.*` caps
//! on the memory peer:
//!
//! - `POST /v1/knowledge/share`
//! - `GET  /v1/knowledge/shared/:agent`
//! - `POST /v1/knowledge/broadcast`
//! - `GET  /v1/knowledge/groups`
//! - `POST /v1/knowledge/revoke`
//!
//! All `POST` endpoints reject empty / invalid payloads with
//! 400 + structured error body BEFORE dialing the mesh.
//! Error mapping mirrors the existing `/v1/memory/*` surface:
//! INVALID_ARGS → 400, peer alias missing → 404, responder
//! fault → 502, mesh client not ready → 503.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject, current_tenant};

const DEFAULT_PEER: &str = "memory";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PeerQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ShareRequest {
    pub source_agent: String,
    pub target_agents: Vec<String>,
    pub observation_ids: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

/// `POST /v1/knowledge/share`
pub async fn share(
    State(state): State<AppState>,
    Json(req): Json<ShareRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.source_agent.trim().is_empty() {
        return bad_request("source_agent is required");
    }
    if req.target_agents.is_empty() {
        return bad_request("target_agents must list at least one agent");
    }
    if req.observation_ids.is_empty() {
        return bad_request("observation_ids must list at least one id");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = share_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("source_agent".into(), Value::from(req.source_agent));
    body.insert("target_agents".into(), Value::from(req.target_agents));
    body.insert("observation_ids".into(), Value::from(req.observation_ids));
    if let Some(m) = req.message {
        body.insert("message".into(), Value::from(m));
    }
    match call_peer_json(
        &state,
        &peer,
        "knowledge.share",
        &Value::Object(body),
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_knowledge_activity(
                &state,
                &peer,
                "knowledge.share",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_knowledge_activity(
                &state,
                &peer,
                "knowledge.share",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ListSharedQuery {
    #[serde(default)]
    pub shared_by: Option<String>,
    #[serde(default)]
    pub date_from: Option<i64>,
    #[serde(default)]
    pub date_to: Option<i64>,
    #[serde(default, alias = "min_quality")]
    pub min_quality_score: Option<f32>,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `GET /v1/knowledge/shared/:agent`
pub async fn list_shared(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(q): Query<ListSharedQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if agent.trim().is_empty() {
        return bad_request("agent path segment is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("agent".into(), Value::from(agent));
    if let Some(v) = q.shared_by {
        body.insert("shared_by".into(), Value::from(v));
    }
    if let Some(v) = q.date_from {
        body.insert("date_from".into(), Value::from(v));
    }
    if let Some(v) = q.date_to {
        body.insert("date_to".into(), Value::from(v));
    }
    if let Some(v) = q.min_quality_score {
        body.insert("min_quality_score".into(), Value::from(v));
    }
    match call_peer_json(
        &state,
        &peer,
        "knowledge.list_shared",
        &Value::Object(body),
        None,
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize)]
pub struct BroadcastRequest {
    pub caller_agent: String,
    pub group: String,
    pub observation_ids: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

/// `POST /v1/knowledge/broadcast`
pub async fn broadcast(
    State(state): State<AppState>,
    Json(req): Json<BroadcastRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.caller_agent.trim().is_empty() {
        return bad_request("caller_agent is required");
    }
    if req.group.trim().is_empty() {
        return bad_request("group is required");
    }
    if req.observation_ids.is_empty() {
        return bad_request("observation_ids must list at least one id");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = broadcast_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("caller_agent".into(), Value::from(req.caller_agent));
    body.insert("group".into(), Value::from(req.group));
    body.insert("observation_ids".into(), Value::from(req.observation_ids));
    if let Some(m) = req.message {
        body.insert("message".into(), Value::from(m));
    }
    match call_peer_json(
        &state,
        &peer,
        "knowledge.group_broadcast",
        &Value::Object(body),
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_knowledge_activity(
                &state,
                &peer,
                "knowledge.group_broadcast",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_knowledge_activity(
                &state,
                &peer,
                "knowledge.group_broadcast",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

/// `GET /v1/knowledge/groups`
pub async fn groups(
    State(state): State<AppState>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "knowledge.groups", &Value::Null, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub observation_ids: Vec<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

/// `POST /v1/knowledge/revoke`
pub async fn revoke(
    State(state): State<AppState>,
    Json(req): Json<RevokeRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.observation_ids.is_empty() {
        return bad_request("observation_ids must list at least one id");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = revoke_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "observation_ids": req.observation_ids });
    match call_peer_json(&state, &peer, "knowledge.revoke", &body, task_id.as_deref()).await {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_knowledge_activity(
                &state,
                &peer,
                "knowledge.revoke",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_knowledge_activity(
                &state,
                &peer,
                "knowledge.revoke",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RecallRequest {
    pub source_agent: String,
    pub source_observation_ids: Vec<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

/// `POST /v1/knowledge/recall` — RELIX-7.16 GAP 2.
pub async fn recall(
    State(state): State<AppState>,
    Json(req): Json<RecallRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.source_agent.trim().is_empty() {
        return bad_request("source_agent is required");
    }
    if req.source_observation_ids.is_empty() {
        return bad_request("source_observation_ids must list at least one id");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = recall_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({
        "source_agent": req.source_agent,
        "source_observation_ids": req.source_observation_ids,
    });
    match call_peer_json(&state, &peer, "knowledge.recall", &body, task_id.as_deref()).await {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_knowledge_activity(
                &state,
                &peer,
                "knowledge.recall",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_knowledge_activity(
                &state,
                &peer,
                "knowledge.recall",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

// ── helpers ──────────────────────────────────────────────

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
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 120);
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
            if body.is_empty() {
                return Ok(Value::Null);
            }
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
                error: "unexpected stream response from memory peer".into(),
            }),
        )
            .into_response()),
    }
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

fn share_detail(req: &ShareRequest) -> String {
    format!(
        "source_agent={}; target_count={}; observation_count={}; message_len={}",
        req.source_agent.trim(),
        req.target_agents.len(),
        req.observation_ids.len(),
        req.message.as_deref().map(str::len).unwrap_or(0)
    )
}

fn broadcast_detail(req: &BroadcastRequest) -> String {
    format!(
        "caller_agent={}; group={}; observation_count={}; message_len={}",
        req.caller_agent.trim(),
        req.group.trim(),
        req.observation_ids.len(),
        req.message.as_deref().map(str::len).unwrap_or(0)
    )
}

fn revoke_detail(req: &RevokeRequest) -> String {
    format!("observation_count={}", req.observation_ids.len())
}

fn recall_detail(req: &RecallRequest) -> String {
    format!(
        "source_agent={}; observation_count={}",
        req.source_agent.trim(),
        req.source_observation_ids.len()
    )
}

fn record_knowledge_activity(
    state: &AppState,
    peer: &str,
    method: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "knowledge".into());
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
        tracing::warn!(error = %e, method, "failed to append knowledge activity");
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn bad_request_returns_400_with_error_body() {
        let resp = bad_request("source_agent is required");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bytes = rt.block_on(async { to_bytes(resp.into_body(), 64_000).await.unwrap() });
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            parsed.get("error").and_then(Value::as_str),
            Some("source_agent is required")
        );
    }

    #[test]
    fn knowledge_mutations_accept_scope_context() {
        let share: ShareRequest = serde_json::from_str(
            r#"{
                "source_agent":"alice",
                "target_agents":["bob"],
                "observation_ids":["obs-1"],
                "message":"private note",
                "task_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id":"run-1"
            }"#,
        )
        .unwrap();
        assert_eq!(
            share.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(share.run_id.as_deref(), Some("run-1"));
        let detail = share_detail(&share);
        assert!(detail.contains("message_len=12"));
        assert!(!detail.contains("private note"));

        let broadcast: BroadcastRequest = serde_json::from_str(
            r#"{
                "caller_agent":"alice",
                "group":"ops",
                "observation_ids":["obs-1"],
                "task_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }"#,
        )
        .unwrap();
        assert_eq!(
            broadcast.task_id.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            broadcast_detail(&broadcast),
            "caller_agent=alice; group=ops; observation_count=1; message_len=0"
        );
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut value = serde_json::json!({ "shared": true });
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
