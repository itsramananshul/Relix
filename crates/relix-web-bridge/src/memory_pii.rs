//! RELIX-7.15 — HTTP proxies for the memory node's PII surface.
//!
//! Three endpoints, all forwarders to the `memory.*` PII caps:
//!
//! - `POST /v1/memory/pii/scan`           → `memory.pii_scan`.
//! - `POST /v1/memory/pii/preview`        → `memory.anonymize_preview`.
//! - `POST /v1/memory/pii/bulk_anonymize` → `memory.bulk_anonymize`.
//!
//! The scan/preview endpoints reject empty-text requests with
//! 400 + structured error body BEFORE dialing the mesh.
//! `bulk_anonymize` takes no body args — operators just POST
//! `{}` (or no body) and the coordinator walks both the turns
//! table and the layered `memory_records` table once.

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "memory";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct PiiScanRequest {
    pub text: String,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `POST /v1/memory/pii/scan`
pub async fn scan(
    State(state): State<AppState>,
    Json(req): Json<PiiScanRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.text.is_empty() {
        return bad_request("text is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "text": req.text });
    match call_peer_json(&state, &peer, "memory.pii_scan", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize)]
pub struct PiiPreviewRequest {
    pub text: String,
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `POST /v1/memory/pii/preview`
pub async fn preview(
    State(state): State<AppState>,
    Json(req): Json<PiiPreviewRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.text.is_empty() {
        return bad_request("text is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("text".into(), Value::from(req.text));
    if let Some(s) = req.strategy {
        body.insert("strategy".into(), Value::from(s));
    }
    match call_peer_json(
        &state,
        &peer,
        "memory.anonymize_preview",
        &Value::Object(body),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct BulkAnonymizeRequest {
    #[serde(default)]
    pub peer: Option<String>,
}

/// `POST /v1/memory/pii/bulk_anonymize`
pub async fn bulk_anonymize(
    State(state): State<AppState>,
    body: Option<Json<BulkAnonymizeRequest>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(
        &state,
        &peer,
        "memory.bulk_anonymize",
        &Value::Object(Default::default()),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn bad_request_returns_400_with_error_body() {
        let resp = bad_request("text is required");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bytes = rt.block_on(async { to_bytes(resp.into_body(), 64_000).await.unwrap() });
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            parsed.get("error").and_then(serde_json::Value::as_str),
            Some("text is required")
        );
    }
}
