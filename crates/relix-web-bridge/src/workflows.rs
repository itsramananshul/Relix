//! HTTP proxies for the workflow engine.
//!
//! Five endpoints, each a thin forwarder to a `workflow.*`
//! coordinator capability:
//!
//! - `POST /v1/workflows/run`                  — execute by name.
//! - `GET  /v1/workflows`                      — list catalog.
//! - `GET  /v1/workflows/status/:execution_id` — fetch past run.
//! - `POST /v1/workflows/validate`             — type-check source.
//! - `POST /v1/workflows/reload`               — drop the file cache.
//!
//! When `POST /v1/workflows/run` is called with `stream:
//! true` the response is a real `text/event-stream` driven
//! by the coordinator's `workflow.run.stream` streaming
//! capability — each emitted event (started / step_started /
//! step_completed / step_failed / finished) becomes one SSE
//! frame in real time as the workflow runs.

use axum::body::Body;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use relix_runtime::transport::stream::{StreamFrame, StreamReader, write_request_envelope};

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
pub async fn run(State(state): State<AppState>, Json(req): Json<RunRequest>) -> Response {
    if req.name.trim().is_empty() {
        return bad_json(StatusCode::BAD_REQUEST, "name is required");
    }
    if req.stream {
        return run_stream(&state, &req).await;
    }
    let coord_args = serde_json::json!({
        "name": req.name,
        "input": req.input,
    });
    let coord_arg_bytes = match serde_json::to_vec(&coord_args) {
        Ok(b) => b,
        Err(e) => return bad_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {e}")),
    };
    match call_peer_json(&state, DEFAULT_PEER, "workflow.run", &coord_arg_bytes).await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(resp) => resp,
    }
}

/// `GET /v1/workflows` — list every workflow in the catalog.
pub async fn list(State(state): State<AppState>) -> Response {
    match call_peer_json(&state, DEFAULT_PEER, "workflow.list", b"").await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(resp) => resp,
    }
}

/// `GET /v1/workflows/status/:execution_id` — fetch a past
/// execution. Returns 404 when the id is unknown.
pub async fn status(State(state): State<AppState>, Path(execution_id): Path<String>) -> Response {
    let coord_args = serde_json::json!({ "execution_id": execution_id });
    let arg_bytes = match serde_json::to_vec(&coord_args) {
        Ok(b) => b,
        Err(e) => return bad_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {e}")),
    };
    let body = match call_peer_json(&state, DEFAULT_PEER, "workflow.status", &arg_bytes).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if body.get("error").is_some() && body.get("execution_id").is_none() {
        return json_response(StatusCode::NOT_FOUND, body);
    }
    json_response(StatusCode::OK, body)
}

/// `POST /v1/workflows/validate` — type-check a workflow source.
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

/// `POST /v1/workflows/reload` — drop the workflow file
/// cache on the coordinator so the next list / run picks up
/// any in-place edits without a coordinator restart.
pub async fn reload(State(state): State<AppState>) -> Response {
    match call_peer_json(&state, DEFAULT_PEER, "workflow.reload", b"").await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(resp) => resp,
    }
}

// ── streaming ────────────────────────────────────────────

async fn run_stream(state: &AppState, req: &RunRequest) -> Response {
    let Some(mesh) = state.mesh_client.as_ref().cloned() else {
        return bad_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge mesh client not initialized",
        );
    };
    let Some(peer_id) = mesh.peer_id_for(DEFAULT_PEER) else {
        return bad_json(
            StatusCode::NOT_FOUND,
            "coordinator peer alias not in peers.toml",
        );
    };
    let coord_args = serde_json::json!({
        "name": req.name,
        "input": req.input,
    });
    let arg_bytes = match serde_json::to_vec(&coord_args) {
        Ok(b) => b,
        Err(e) => return bad_json(StatusCode::INTERNAL_SERVER_ERROR, &format!("encode: {e}")),
    };
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 600);
    let envelope = build_request_with_tenant(
        "workflow.run.stream",
        arg_bytes,
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
    );

    let mut raw_stream = match mesh.client().open_stream(peer_id).await {
        Ok(s) => s,
        Err(e) => {
            return bad_json(
                StatusCode::BAD_GATEWAY,
                &format!("open stream to coordinator: {e}"),
            );
        }
    };
    if let Err(e) = write_request_envelope(&mut raw_stream, &envelope).await {
        return bad_json(
            StatusCode::BAD_GATEWAY,
            &format!("write workflow.run.stream request: {e}"),
        );
    }
    let reader = StreamReader::new(raw_stream);

    let sse_body = async_stream::stream! {
        let mut reader = reader;
        loop {
            match reader.next_frame().await {
                Ok(Some(StreamFrame::Header { .. })) => {
                    // Headers carry audit metadata, not a step
                    // event — drop quietly.
                    continue;
                }
                Ok(Some(StreamFrame::Chunk(bytes))) => {
                    let payload = String::from_utf8_lossy(&bytes).to_string();
                    let event_name = parse_event_name(&payload).unwrap_or("message".to_string());
                    let frame = format!("event: {event_name}\ndata: {payload}\n\n");
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from(frame));
                }
                Ok(Some(StreamFrame::End)) | Ok(None) => {
                    break;
                }
                Ok(Some(StreamFrame::Err { kind, cause })) => {
                    let payload = serde_json::json!({
                        "event": "error",
                        "kind": kind,
                        "error": cause,
                    });
                    let frame = format!(
                        "event: error\ndata: {}\n\n",
                        serde_json::to_string(&payload).unwrap_or_default(),
                    );
                    yield Ok(bytes::Bytes::from(frame));
                    break;
                }
                Err(e) => {
                    let payload = serde_json::json!({
                        "event": "error",
                        "error": format!("stream read: {e}"),
                    });
                    let frame = format!(
                        "event: error\ndata: {}\n\n",
                        serde_json::to_string(&payload).unwrap_or_default(),
                    );
                    yield Ok(bytes::Bytes::from(frame));
                    break;
                }
            }
        }
    };

    let body = Body::from_stream(sse_body);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    (StatusCode::OK, headers, body).into_response()
}

/// Pull the `event` field out of a JSON payload chunk so we
/// can stamp the SSE frame's `event:` line. The coordinator
/// emits `{"event": "<name>", ...}` for every workflow
/// event; if a chunk doesn't conform we fall back to the
/// default `message` event-name (still valid SSE).
fn parse_event_name(payload: &str) -> Option<String> {
    let v: Value = serde_json::from_str(payload).ok()?;
    v.get("event").and_then(Value::as_str).map(String::from)
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
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
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
    fn parses_event_name_from_well_formed_payload() {
        let name = parse_event_name(r#"{"event":"step_started","agent":"a"}"#);
        assert_eq!(name.as_deref(), Some("step_started"));
    }

    #[test]
    fn falls_back_when_payload_has_no_event_field() {
        assert!(parse_event_name(r#"{"agent":"a"}"#).is_none());
        assert!(parse_event_name("not even json").is_none());
    }
}
