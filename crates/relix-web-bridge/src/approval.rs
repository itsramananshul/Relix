//! HTTP proxies for the approval-delivery surface.
//!
//! - `GET /v1/approval/:id/delivery` → `approval.delivery_status`
//!   (RELIX-7.30 PART 1)
//! - `GET /v1/approval/failed-deliveries` →
//!   `approval.failed_deliveries` (PART 6)

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
pub struct DeliveryQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

/// `GET /v1/approval/:id/delivery`
pub async fn delivery_status(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Query(q): Query<DeliveryQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if approval_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "approval_id is required".into(),
            }),
        )
            .into_response();
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "approval_id": approval_id });
    match call_peer_json(&state, &peer, "approval.delivery_status", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// PART 6 — `GET /v1/approval/failed-deliveries?limit=...&peer=...`
///
/// Lists the rows that landed in `delivery_failed` state on
/// the coordinator's delivery store, newest-first. `limit`
/// defaults to 50; the coordinator caps it at 500. Operators
/// use this to reconcile approvals whose channel send
/// returned an error (Telegram 5xx, Slack `not_in_channel`,
/// SMTP refused, …).
#[derive(Debug, Deserialize, Default)]
pub struct FailedDeliveriesQuery {
    /// Override the responder peer. Defaults to `coordinator`.
    #[serde(default)]
    pub peer: Option<String>,
    /// Max rows to return. Server-side clamp `[1, 500]`.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Handler for `GET /v1/approval/failed-deliveries`.
pub async fn failed_deliveries(
    State(state): State<AppState>,
    Query(q): Query<FailedDeliveriesQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = match q.limit {
        Some(l) => serde_json::json!({ "limit": l }),
        None => serde_json::json!({}),
    };
    match call_peer_json(&state, &peer, "approval.failed_deliveries", &body).await {
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
