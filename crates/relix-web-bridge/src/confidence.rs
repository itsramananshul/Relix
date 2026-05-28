//! RELIX-7.19 — HTTP proxies for the `confidence.*` capability
//! surface.
//!
//! Three endpoints, each a thin forwarder to a `confidence.*`
//! capability on the coordinator:
//!
//! - `GET  /v1/confidence/policies`            — `confidence.policy_list`
//! - `GET  /v1/confidence/history/:agent`      — `confidence.score_history`
//! - `POST /v1/confidence/reset`               — `confidence.reset_history`
//!
//! Error mapping mirrors the metrics endpoints:
//! - `INVALID_ARGS` from the responder → `400 Bad Request`
//! - peer alias missing → `404 Not Found`
//! - responder fault → `502 Bad Gateway`
//! - bridge mesh client not ready → `503 Service Unavailable`

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
pub struct PolicyListQuery {
    /// Override the coordinator peer alias. Default
    /// `"coordinator"`.
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HistoryQuery {
    /// The capability method to read history for.
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResetRequest {
    pub agent: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `GET /v1/confidence/policies`
pub async fn policies(
    State(state): State<AppState>,
    Query(q): Query<PolicyListQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "confidence.policy_list", &Value::Null).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/confidence/history/:agent?method=ai.chat`
pub async fn history(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if agent.trim().is_empty() {
        return bad_request("agent is required");
    }
    let method = match q.method.as_deref() {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => return bad_request("query parameter `method` is required"),
    };
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "agent": agent, "method": method });
    match call_peer_json(&state, &peer, "confidence.score_history", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/confidence/reset`
///
/// Body: `{ "agent": "alice", "method": "ai.chat" }` — when
/// `method` is omitted, every method under that agent is
/// cleared. `peer` overrides the default coordinator alias.
pub async fn reset(
    State(state): State<AppState>,
    Json(req): Json<ResetRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.agent.trim().is_empty() {
        return bad_request("agent is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("agent".into(), Value::from(req.agent));
    if let Some(m) = req.method
        && !m.trim().is_empty()
    {
        body.insert("method".into(), Value::from(m));
    }
    match call_peer_json(
        &state,
        &peer,
        "confidence.reset_history",
        &Value::Object(body),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
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
    fn reset_request_accepts_agent_only() {
        let r: ResetRequest = serde_json::from_str(r#"{"agent":"alice"}"#).expect("parse");
        assert_eq!(r.agent, "alice");
        assert!(r.method.is_none());
    }

    #[test]
    fn reset_request_accepts_agent_and_method() {
        let r: ResetRequest =
            serde_json::from_str(r#"{"agent":"alice","method":"ai.chat"}"#).expect("parse");
        assert_eq!(r.method.as_deref(), Some("ai.chat"));
    }
}
