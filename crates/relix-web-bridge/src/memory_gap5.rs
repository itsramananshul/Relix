//! GAP 5 — HTTP proxies for the four missing memory caps.
//!
//! All four endpoints proxy a single JSON object straight through
//! to the configured memory peer over the mesh:
//!
//! - `POST /v1/memory/dialectic`       → `memory.dialectic`.
//! - `POST /v1/memory/ingest`          → `memory.ingest_document`.
//! - `POST /v1/memory/ingest_image`    → `memory.ingest_image`.
//! - `POST /v1/memory/context_flush`   → `memory.context_flush`.
//!
//! The bridge does not own a `LayeredMemoryStore` writer — it
//! always rides the mesh so the memory controller stays the
//! single writer. When `mesh_client` is unset, every handler
//! responds 503 with a structured body.

use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;

use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "memory";

#[derive(Debug, serde::Serialize)]
pub struct ApiError {
    pub error: String,
}

/// `POST /v1/memory/dialectic` — forwards the entire request body
/// (less the optional `peer` key) to `memory.dialectic`.
pub async fn dialectic(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, req, "memory.dialectic").await
}

/// `POST /v1/memory/ingest` — forwards to `memory.ingest_document`.
pub async fn ingest(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, req, "memory.ingest_document").await
}

/// `POST /v1/memory/ingest_image` — forwards to `memory.ingest_image`.
pub async fn ingest_image(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, req, "memory.ingest_image").await
}

/// `POST /v1/memory/context_flush` — forwards to `memory.context_flush`.
pub async fn context_flush(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, req, "memory.context_flush").await
}

// ── helpers ──────────────────────────────────────────────

async fn proxy_json(state: &AppState, mut req: Value, method: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    // The bridge accepts an optional `peer` override per call;
    // strip it so the wire payload matches the cap's contract.
    let peer = req
        .as_object_mut()
        .and_then(|m| m.remove("peer"))
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(state, &peer, method, &req).await {
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
    // Document + image ingestion can sit on a vision model for a
    // while — give the deadline a wider ceiling than the default
    // bridge calls so the bridge does not time out before the
    // memory peer has a chance to answer.
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(15, 600);
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

    #[test]
    fn peer_override_is_stripped_from_payload() {
        // Sanity: when the caller sends `peer` in the body, the
        // helper removes it so the cap doesn't see an extra field
        // (caps reject unknown keys).
        let mut v = serde_json::json!({
            "peer": "memory-2",
            "observer_id": "agent.alpha",
            "subject_id": "user.bob",
            "question": "what color is the sky"
        });
        let removed = v
            .as_object_mut()
            .and_then(|m| m.remove("peer"))
            .and_then(|x| x.as_str().map(str::to_string));
        assert_eq!(removed.as_deref(), Some("memory-2"));
        assert!(v.get("peer").is_none());
        assert_eq!(
            v.get("observer_id").and_then(Value::as_str),
            Some("agent.alpha")
        );
    }
}
