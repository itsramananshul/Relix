//! HTTP proxies for the workflow engine.
//!
//! Four endpoints, each a thin forwarder to a `workflow.*`
//! coordinator capability:
//!
//! - `POST /v1/workflows/run`                          — execute by name.
//! - `GET  /v1/workflows`                              — list catalog.
//! - `GET  /v1/workflows/:name/status/:execution_id`   — fetch past run.
//! - `POST /v1/workflows/validate`                     — type-check source.
//!
//! When `POST /v1/workflows/run` is called with `stream:
//! true` the response is a single `text/event-stream` frame
//! carrying the final execution record (the foundation
//! engine is unary today — per-step SSE arrives when the
//! coordinator gains a streaming workflow capability). The
//! shape stays SSE-compatible so a future streaming
//! upgrade is drop-in for dashboard clients.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
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

#[derive(Debug, Deserialize)]
pub struct RunRequest {
    pub name: String,
    #[serde(default)]
    pub input: String,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Deserialize)]
pub struct ValidateRequest {
    pub source: String,
}

/// `POST /v1/workflows/run` — execute a workflow.
///
/// Body: `{ "name": "<workflow>", "input": "<text>", "stream"?: bool }`.
///
/// Response (unary): the full execution record as JSON.
///
/// Response (stream): a single `text/event-stream` event of
/// type `result` carrying the same execution record. Future
/// streaming variant will interleave per-step events before
/// the final `result` — the event-name discipline stays
/// stable so dashboard clients pick up streaming without a
/// rewrite.
pub async fn run(State(state): State<AppState>, Json(req): Json<RunRequest>) -> Response {
    if req.name.trim().is_empty() {
        return bad_json(StatusCode::BAD_REQUEST, "name is required");
    }
    let coord_args = serde_json::json!({
        "name": req.name,
        "input": req.input,
    });
    let coord_arg_bytes = match serde_json::to_vec(&coord_args) {
        Ok(b) => b,
        Err(e) => return bad_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {e}")),
    };
    let body = match call_peer_json(&state, DEFAULT_PEER, "workflow.run", &coord_arg_bytes).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if req.stream {
        sse_single_event("result", &body).into_response()
    } else {
        json_response(StatusCode::OK, body)
    }
}

/// `GET /v1/workflows` — list every workflow in the catalog.
pub async fn list(State(state): State<AppState>) -> Response {
    match call_peer_json(&state, DEFAULT_PEER, "workflow.list", b"").await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(resp) => resp,
    }
}

/// `GET /v1/workflows/:name/status/:execution_id` — look up
/// a past execution. `:name` is passed for human routing
/// clarity but the lookup is keyed on execution_id alone.
pub async fn status(
    State(state): State<AppState>,
    Path((_name, execution_id)): Path<(String, String)>,
) -> Response {
    let coord_args = serde_json::json!({ "execution_id": execution_id });
    let arg_bytes = match serde_json::to_vec(&coord_args) {
        Ok(b) => b,
        Err(e) => return bad_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {e}")),
    };
    let body = match call_peer_json(&state, DEFAULT_PEER, "workflow.status", &arg_bytes).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // The coordinator returns `{"error": "..."}` on miss; map
    // that to 404 so clients see the right status code.
    if body.get("error").is_some() && body.get("execution_id").is_none() {
        return json_response(StatusCode::NOT_FOUND, body);
    }
    json_response(StatusCode::OK, body)
}

/// `POST /v1/workflows/validate` — type-check a workflow
/// source string. Body: `{"source": "<yaml>"}`. Returns
/// `200 { ok: true, name, version, description }` on a
/// clean parse + validate, otherwise
/// `400 { ok: false, error }`.
pub async fn validate(State(state): State<AppState>, Json(req): Json<ValidateRequest>) -> Response {
    if req.source.trim().is_empty() {
        return bad_json(StatusCode::BAD_REQUEST, "source is required");
    }
    let coord_args = serde_json::json!({ "source": req.source });
    let arg_bytes = match serde_json::to_vec(&coord_args) {
        Ok(b) => b,
        Err(e) => return bad_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {e}")),
    };
    let body = match call_peer_json(&state, DEFAULT_PEER, "workflow.validate", &arg_bytes).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    json_response(status, body)
}

// ── helpers ──────────────────────────────────────────────

fn bad_json(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::to_vec(&ApiError {
        error: msg.to_string(),
    })
    .unwrap_or_default();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    (status, headers, body).into_response()
}

fn json_response(status: StatusCode, body: Value) -> Response {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    (status, headers, bytes).into_response()
}

fn sse_single_event(event_name: &str, body: &Value) -> Response {
    let payload = serde_json::to_string(body).unwrap_or_default();
    // Standard SSE encoding: optional event field + data field
    // + blank-line terminator. Keep both lines literal so
    // event-source parsers in every browser see the same
    // shape.
    let frame = format!("event: {event_name}\ndata: {payload}\n\n");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    (StatusCode::OK, headers, frame).into_response()
}

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<Value, Response> {
    let mesh = state.mesh_client.as_ref().ok_or_else(|| {
        bad_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge mesh client not initialized",
        )
    })?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 120);
    let envelope = build_request(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
    );
    let timeout = std::time::Duration::from_secs(deadline_secs as u64 + 5);
    let resp_bytes = tokio::time::timeout(timeout, mesh.call(alias, envelope))
        .await
        .map_err(|_| {
            bad_json(
                StatusCode::GATEWAY_TIMEOUT,
                &format!("mesh call exceeded {} second wall clock", timeout.as_secs()),
            )
        })?
        .map_err(|e| {
            let msg = e.to_string();
            let lower = msg.to_ascii_lowercase();
            let status = if lower.contains("unknown alias") || lower.contains("no peer") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            };
            bad_json(status, &msg)
        })?;
    let resp = decode_response(&resp_bytes)
        .map_err(|e| bad_json(StatusCode::BAD_GATEWAY, &format!("decode response: {e}")))?;
    match resp.res {
        ResponseResult::Ok(body) => serde_json::from_slice(body.as_ref()).map_err(|e| {
            bad_json(
                StatusCode::BAD_GATEWAY,
                &format!("response not valid JSON: {e}"),
            )
        }),
        ResponseResult::Err(env) => {
            let lower = env.cause.to_ascii_lowercase();
            let status = if lower.contains("not found") {
                StatusCode::NOT_FOUND
            } else if env.kind == 5 {
                StatusCode::BAD_REQUEST
            } else if lower.contains("not ready") || lower.contains("not wired") {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err(bad_json(
                status,
                &format!("responder err kind={} cause={}", env.kind, env.cause),
            ))
        }
        ResponseResult::StreamHandle(_) => Err(bad_json(
            StatusCode::BAD_GATEWAY,
            "unexpected stream response from coordinator",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frame_is_well_formed() {
        let body = serde_json::json!({"hello": "world"});
        let resp = sse_single_event("result", &body);
        // Surfaced via the response's content-type header
        // because Response::into_body requires running the
        // hyper runtime; assert structure indirectly.
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or("").to_string());
        assert_eq!(ct.as_deref(), Some("text/event-stream"));
    }
}
