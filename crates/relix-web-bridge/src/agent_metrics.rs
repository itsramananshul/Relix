//! RELIX-7.11 — HTTP proxies for the agent metrics surface.
//!
//! Six endpoints, each a thin forwarder to a `metrics.*`
//! capability on the coordinator:
//!
//! - `GET  /v1/metrics/agents`                       — `metrics.agents`
//! - `GET  /v1/metrics/agents/:agent/summary`        — `metrics.agent_summary`
//! - `GET  /v1/metrics/agents/:agent/methods`        — `metrics.method_breakdown`
//! - `GET  /v1/metrics/agents/:agent/timeseries`     — `metrics.timeseries`
//! - `GET  /v1/metrics/alerts`                       — `metrics.alerts_active`
//! - `GET  /v1/metrics/cost`                         — `metrics.cost_report`
//!
//! Query parameters:
//!
//! - `hours` (default 24)
//! - `bucket_minutes` (default 5; only honoured by `/timeseries`)
//!
//! Error mapping mirrors the workflow endpoints:
//! - `INVALID_ARGS` from the responder → `400 Bad Request`
//! - peer alias missing → `404 Not Found`
//! - responder fault → `502 Bad Gateway`
//! - bridge mesh client not ready → `503 Service Unavailable`
//!
//! When an agent has no metrics in the window, the responder
//! returns a non-error empty summary; the bridge converts that
//! to `404 Not Found` per the spec.

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
pub struct CommonQuery {
    #[serde(default)]
    pub hours: Option<u32>,
    #[serde(default)]
    pub bucket_minutes: Option<u32>,
    /// Override the coordinator peer alias. Default
    /// `"coordinator"`.
    #[serde(default)]
    pub peer: Option<String>,
}

/// `GET /v1/metrics/agents` — list every agent with metrics in
/// the last `hours` window (default 24).
pub async fn list_agents(
    State(state): State<AppState>,
    Query(q): Query<CommonQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "hours": q.hours.unwrap_or(24) });
    match call_peer_json(&state, &peer, "metrics.agents", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/metrics/agents/:agent/summary`
pub async fn agent_summary(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(q): Query<CommonQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if agent.trim().is_empty() {
        return bad_request("agent is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({
        "agent": agent,
        "hours": q.hours.unwrap_or(24),
    });
    let v = match call_peer_json(&state, &peer, "metrics.agent_summary", &body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // Empty-window detection: responder returns a valid summary
    // with invocations = 0 → bridge converts to 404 per spec.
    if v.get("invocations").and_then(Value::as_u64) == Some(0) {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("no metrics for agent {agent:?} in the requested window"),
            }),
        )
            .into_response();
    }
    (StatusCode::OK, Json(v)).into_response()
}

/// `GET /v1/metrics/agents/:agent/methods`
pub async fn agent_methods(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(q): Query<CommonQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if agent.trim().is_empty() {
        return bad_request("agent is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({
        "agent": agent,
        "hours": q.hours.unwrap_or(24),
    });
    let v = match call_peer_json(&state, &peer, "metrics.method_breakdown", &body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // Empty array → 404.
    if v.as_array().map(|a| a.is_empty()).unwrap_or(false) {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("no methods for agent {agent:?} in the requested window"),
            }),
        )
            .into_response();
    }
    (StatusCode::OK, Json(v)).into_response()
}

/// `GET /v1/metrics/agents/:agent/timeseries`
pub async fn agent_timeseries(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(q): Query<CommonQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if agent.trim().is_empty() {
        return bad_request("agent is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({
        "agent": agent,
        "hours": q.hours.unwrap_or(24),
        "bucket_minutes": q.bucket_minutes.unwrap_or(5),
    });
    let v = match call_peer_json(&state, &peer, "metrics.timeseries", &body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // Timeseries can return an empty array (no buckets in the
    // window). We treat that as 404 to match the rest of the
    // surface — the dashboard shouldn't render an empty chart.
    let any_invocations = v
        .as_array()
        .map(|a| a.iter().any(|b| b.get("invocations").and_then(Value::as_u64).unwrap_or(0) > 0))
        .unwrap_or(false);
    if !any_invocations {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("no timeseries data for agent {agent:?} in the requested window"),
            }),
        )
            .into_response();
    }
    (StatusCode::OK, Json(v)).into_response()
}

/// `GET /v1/metrics/alerts`
pub async fn alerts(
    State(state): State<AppState>,
    Query(q): Query<CommonQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "metrics.alerts_active", &serde_json::json!({})).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/metrics/cost`
pub async fn cost(
    State(state): State<AppState>,
    Query(q): Query<CommonQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "hours": q.hours.unwrap_or(24) });
    match call_peer_json(&state, &peer, "metrics.cost_report", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

// ── mesh helpers ─────────────────────────────────────────

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
    fn common_query_default_is_all_none() {
        let q = CommonQuery::default();
        assert!(q.hours.is_none());
        assert!(q.bucket_minutes.is_none());
        assert!(q.peer.is_none());
    }

    #[test]
    fn bad_request_returns_400_with_error_body() {
        use axum::body::to_bytes;
        let resp = bad_request("agent is required");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Drain the body to confirm the JSON shape — uses
        // `to_bytes` from axum-body so we don't pull in tower
        // crates manually. Test is sync so we use an in-place
        // runtime.
        let body = resp.into_body();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bytes = rt
            .block_on(async move { to_bytes(body, 64_000).await.unwrap() });
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            parsed.get("error").and_then(serde_json::Value::as_str),
            Some("agent is required")
        );
    }
}
