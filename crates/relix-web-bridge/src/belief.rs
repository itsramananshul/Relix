//! RELIX-7.29 PART 3 — HTTP proxies for the `belief.*` caps.
//!
//! Two endpoints:
//!
//! - `GET  /v1/belief/:session_id` → `belief.get`
//! - `POST /v1/belief/:session_id` with `{"action":"reset"}` → `belief.reset`
//!
//! `subject_id` is optional on both — defaults to the bridge
//! identity's subject. `peer` overrides the coordinator alias.

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
pub struct GetQuery {
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PostBody {
    /// Only `reset` is currently honoured.
    pub action: String,
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `GET /v1/belief/:session_id`
pub async fn get(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(q): Query<GetQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if session_id.trim().is_empty() {
        return bad_request("session_id is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("session_id".into(), Value::from(session_id));
    if let Some(s) = q.subject_id.as_ref() {
        body.insert("subject_id".into(), Value::from(s.clone()));
    }
    match call_peer_json(&state, &peer, "belief.get", &Value::Object(body)).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/belief/:session_id`
pub async fn post(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<PostBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if session_id.trim().is_empty() {
        return bad_request("session_id is required");
    }
    if !req.action.trim().eq_ignore_ascii_case("reset") {
        return bad_request("action must be \"reset\"");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("session_id".into(), Value::from(session_id));
    if let Some(s) = req.subject_id.as_ref() {
        body.insert("subject_id".into(), Value::from(s.clone()));
    }
    match call_peer_json(&state, &peer, "belief.reset", &Value::Object(body)).await {
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
