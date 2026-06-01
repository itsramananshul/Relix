//! RELIX-7.30 PART 3 — HTTP proxies for the session-identity
//! `identity.*` caps.
//!
//! - `POST /v1/identity/tokens`        → `identity.issue_token`
//! - `POST /v1/identity/tokens/verify` → `identity.verify_token`
//! - `POST /v1/identity/tokens/revoke` → `identity.revoke_token`
//! - `GET  /v1/identity/tokens`        → `identity.active_tokens`
//!
//! RELIX-7.18 / GAP 17 PART 2 — research-backed identity:
//!
//! - `POST /v1/identity/research`      → `identity.research`

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IssueBody {
    pub session_id: String,
    pub agent_name: String,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VerifyBody {
    pub token: String,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevokeBody {
    pub session_id: String,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResearchBody {
    pub subject_name: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

pub async fn issue(
    State(state): State<AppState>,
    Json(req): Json<IssueBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.session_id.trim().is_empty() || req.agent_name.trim().is_empty() {
        return bad_request("session_id and agent_name are required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("session_id".into(), Value::from(req.session_id));
    body.insert("agent_name".into(), Value::from(req.agent_name));
    if let Some(t) = req.tenant_id {
        body.insert("tenant_id".into(), Value::from(t));
    }
    body.insert("scopes".into(), Value::from(req.scopes));
    if let Some(ttl) = req.ttl_secs {
        body.insert("ttl_secs".into(), Value::from(ttl));
    }
    match call_peer_json(&state, &peer, "identity.issue_token", &Value::Object(body)).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn verify(
    State(state): State<AppState>,
    Json(req): Json<VerifyBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.token.trim().is_empty() {
        return bad_request("token is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "token": req.token });
    match call_peer_json(&state, &peer, "identity.verify_token", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn revoke(
    State(state): State<AppState>,
    Json(req): Json<RevokeBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.session_id.trim().is_empty() {
        return bad_request("session_id is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "session_id": req.session_id });
    match call_peer_json(&state, &peer, "identity.revoke_token", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn research(
    State(state): State<AppState>,
    Json(req): Json<ResearchBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.subject_name.trim().is_empty() {
        return bad_request("subject_name is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("subject_name".into(), Value::from(req.subject_name));
    if let Some(c) = req.context {
        body.insert("context".into(), Value::from(c));
    }
    // The pipeline's approval gate can wait up to 5 minutes;
    // give the mesh call a 600s envelope so a slow operator
    // doesn't cap the synthesis before the gate finishes.
    match call_peer_json_with_deadline(
        &state,
        &peer,
        "identity.research",
        &Value::Object(body),
        600_i64,
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(a) = q.agent_name {
        body.insert("agent_name".into(), Value::from(a));
    }
    match call_peer_json(
        &state,
        &peer,
        "identity.active_tokens",
        &Value::Object(body),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

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
/// reports UNKNOWN_METHOD (session-identity caps not registered), so
/// the panel renders an empty state instead of a 502.
fn unavailable(method: &str) -> Value {
    serde_json::json!({
        "available": false,
        "reason": format!("capability '{method}' is not enabled on this deployment"),
    })
}

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
) -> Result<Value, axum::response::Response> {
    let deadline = state.cfg.transport.deadline_secs.clamp(5, 120);
    call_peer_json_with_deadline(state, alias, method, args, deadline).await
}

async fn call_peer_json_with_deadline(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
    deadline_secs: i64,
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
    let envelope = build_request_with_tenant(
        method,
        arg_bytes,
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
            } else if env.kind == relix_core::types::error_kinds::SECURITY_DENIED {
                StatusCode::FORBIDDEN
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
