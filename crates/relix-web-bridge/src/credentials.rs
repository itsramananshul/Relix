//! RELIX-7.30 PART 2 — HTTP proxies for the `credentials.*`
//! caps.
//!
//! - `POST   /v1/credentials`              → `credentials.store`
//! - `GET    /v1/credentials`              → `credentials.list`
//! - `GET    /v1/credentials/:name`        → `credentials.get`
//! - `POST   /v1/credentials/:name/rotate` → `credentials.rotate`
//! - `POST   /v1/credentials/:name/revoke` → `credentials.revoke`
//! - `GET    /v1/credentials/:name/audit`  → `credentials.audit`

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PeerQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub owner_agent: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct StoreBody {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub owner_agent: Option<String>,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub rotation_interval_secs: Option<u64>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RotateBody {
    pub new_value: String,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevokeBody {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

pub async fn store(
    State(state): State<AppState>,
    Json(req): Json<StoreBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.name.trim().is_empty() || req.value.is_empty() {
        return bad_request("name and value are required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("name".into(), Value::from(req.name));
    body.insert("value".into(), Value::from(req.value));
    if let Some(k) = req.kind {
        body.insert("kind".into(), Value::from(k));
    }
    if let Some(o) = req.owner_agent {
        body.insert("owner_agent".into(), Value::from(o));
    }
    if let Some(e) = req.expires_at_ms {
        body.insert("expires_at_ms".into(), Value::from(e));
    }
    if let Some(r) = req.rotation_interval_secs {
        body.insert("rotation_interval_secs".into(), Value::from(r));
    }
    match call_peer_json(&state, &peer, "credentials.store", &Value::Object(body)).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(o) = q.owner_agent {
        body.insert("owner_agent".into(), Value::from(o));
    }
    match call_peer_json(&state, &peer, "credentials.list", &Value::Object(body)).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if name.trim().is_empty() {
        return bad_request("name is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "name": name });
    match call_peer_json(&state, &peer, "credentials.get", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn rotate(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RotateBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if name.trim().is_empty() || req.new_value.is_empty() {
        return bad_request("name and new_value are required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "name": name, "new_value": req.new_value });
    match call_peer_json(&state, &peer, "credentials.rotate", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn revoke(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RevokeBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if name.trim().is_empty() {
        return bad_request("name is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("name".into(), Value::from(name));
    if let Some(r) = req.reason {
        body.insert("reason".into(), Value::from(r));
    }
    match call_peer_json(&state, &peer, "credentials.revoke", &Value::Object(body)).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn audit(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if name.trim().is_empty() {
        return bad_request("name is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("name".into(), Value::from(name));
    if let Some(l) = q.limit {
        body.insert("limit".into(), Value::from(l as u64));
    }
    match call_peer_json(&state, &peer, "credentials.audit", &Value::Object(body)).await {
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

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
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
    let envelope = build_request(
        method,
        arg_bytes,
        state.identity_bundle.clone(),
        deadline_secs,
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
