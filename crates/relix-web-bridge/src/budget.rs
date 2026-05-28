//! RELIX-7.28 Part 1 — HTTP proxies for the budget surface.
//!
//! - `GET  /v1/budget/status` — `budget.status`
//! - `POST /v1/budget/reset`  — `budget.reset`

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
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
pub struct StatusQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResetBody {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    pub window: String,
}

pub async fn status(
    State(state): State<AppState>,
    Query(q): Query<StatusQuery>,
) -> axum::response::Response {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "budget.status", &Value::Null).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn reset(
    State(state): State<AppState>,
    Json(body): Json<ResetBody>,
) -> axum::response::Response {
    let peer = body.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut args = serde_json::Map::new();
    if let Some(a) = body.agent {
        args.insert("agent".into(), Value::String(a));
    }
    args.insert("window".into(), Value::String(body.window));
    match call_peer_json(&state, &peer, "budget.reset", &Value::Object(args)).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
) -> Result<Value, axum::response::Response> {
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
    fn status_query_defaults_to_no_peer_override() {
        let q = StatusQuery::default();
        assert!(q.peer.is_none());
    }
}
