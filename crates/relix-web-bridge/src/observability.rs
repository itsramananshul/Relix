//! RELIX-7.28 Part 2 — HTTP proxies for the observability dashboard.
//!
//! Three thin forwarders onto coordinator capabilities:
//!
//! - `GET /v1/observability/alerts`         — `observability.active_alerts`
//! - `GET /v1/observability/alerts/history` — `observability.alert_history`
//! - `GET /v1/observability/health`         — `observability.health_summary`
//!
//! Error mapping mirrors the metrics endpoints: invalid args → 400,
//! responder fault → 502, bridge mesh not initialised → 503.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ActiveAlertsQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AlertHistoryQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HealthQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub hours: Option<u32>,
}

/// `GET /v1/observability/alerts` — every currently-firing alert.
pub async fn active_alerts(
    State(state): State<AppState>,
    Query(q): Query<ActiveAlertsQuery>,
) -> axum::response::Response {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "observability.active_alerts", &Value::Null).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/observability/alerts/history` — recent alert chronicle rows.
pub async fn alert_history(
    State(state): State<AppState>,
    Query(q): Query<AlertHistoryQuery>,
) -> axum::response::Response {
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(l) = q.limit {
        body.insert("limit".into(), Value::from(l));
    }
    if let Some(a) = q.agent.clone() {
        body.insert("agent".into(), Value::String(a));
    }
    match call_peer_json(
        &state,
        &peer,
        "observability.alert_history",
        &Value::Object(body),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/observability/health` — per-agent + deployment health roll-up.
pub async fn health(
    State(state): State<AppState>,
    Query(q): Query<HealthQuery>,
) -> axum::response::Response {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = if let Some(h) = q.hours {
        serde_json::json!({ "hours": h })
    } else {
        serde_json::json!({})
    };
    match call_peer_json(&state, &peer, "observability.health_summary", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// Clean "feature not enabled" body returned (HTTP 200) when the
/// responder reports UNKNOWN_METHOD for a read-only dashboard call.
/// The dashboard renders panels with `available:false` as an empty /
/// unavailable state rather than a 502 error box.
fn unavailable(method: &str) -> Value {
    serde_json::json!({
        "available": false,
        "reason": format!("capability '{method}' is not enabled on this deployment"),
    })
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
            // A responder that has not registered this capability
            // returns UNKNOWN_METHOD — the optional feature backing it
            // is simply not enabled on this deployment (a default
            // `relix boot` does not configure the observability
            // surface). For these read-only dashboard forwarders,
            // translate that into a clean "unavailable" marker (HTTP
            // 200) so the panel renders an empty state instead of a 502
            // error box. This does NOT weaken admission: the responder
            // still refused the call.
            if env.kind == relix_core::types::error_kinds::UNKNOWN_METHOD {
                return Ok(unavailable(method));
            }
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
    fn active_alerts_query_default_is_all_none() {
        let q = ActiveAlertsQuery::default();
        assert!(q.peer.is_none());
    }

    #[test]
    fn alert_history_query_default_is_all_none() {
        let q = AlertHistoryQuery::default();
        assert!(q.peer.is_none());
        assert!(q.limit.is_none());
        assert!(q.agent.is_none());
    }

    #[test]
    fn health_query_default_is_all_none() {
        let q = HealthQuery::default();
        assert!(q.peer.is_none());
        assert!(q.hours.is_none());
    }
}
