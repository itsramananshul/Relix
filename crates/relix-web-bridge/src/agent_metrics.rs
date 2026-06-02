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
use std::path::Path as FsPath;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::{
    activity::{CostReportActivity, append_cost_report_activity},
    config::AppState,
    tenant::{DEFAULT_TENANT, current_subject, current_tenant},
};

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

#[derive(Debug, Deserialize, Default)]
pub struct CostBaselinesQuery {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub windows: Option<u32>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AskHumanBaselinesQuery {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub windows: Option<u32>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SpikeHistoryQuery {
    #[serde(default)]
    pub limit: Option<u32>,
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
        .map(|a| {
            a.iter()
                .any(|b| b.get("invocations").and_then(Value::as_u64).unwrap_or(0) > 0)
        })
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
    match call_peer_json(
        &state,
        &peer,
        "metrics.alerts_active",
        &serde_json::json!({}),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/metrics/cost-baselines` — GAP 22 Feature 2 baselines.
pub async fn cost_baselines(
    State(state): State<AppState>,
    Query(q): Query<CostBaselinesQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(p) = q.provider {
        body.insert("provider".into(), Value::from(p));
    }
    body.insert(
        "last_n_windows".into(),
        Value::from(q.windows.unwrap_or(24)),
    );
    match call_peer_json(
        &state,
        &peer,
        "metrics.cost_baselines",
        &Value::Object(body),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/metrics/ask-human-baselines` — GAP 22 Feature 2 baselines.
pub async fn ask_human_baselines(
    State(state): State<AppState>,
    Query(q): Query<AskHumanBaselinesQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(a) = q.agent {
        body.insert("agent".into(), Value::from(a));
    }
    body.insert(
        "last_n_windows".into(),
        Value::from(q.windows.unwrap_or(24)),
    );
    match call_peer_json(
        &state,
        &peer,
        "metrics.ask_human_baselines",
        &Value::Object(body),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/metrics/cost-spikes` — GAP 22 Feature 2 spike history.
pub async fn cost_spikes(
    State(state): State<AppState>,
    Query(q): Query<SpikeHistoryQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "limit": q.limit.unwrap_or(20) });
    match call_peer_json(&state, &peer, "metrics.cost_spike_history", &body).await {
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
    let hours = q.hours.unwrap_or(24);
    let body = serde_json::json!({ "hours": hours });
    match call_peer_json(&state, &peer, "metrics.cost_report", &body).await {
        Ok(v) => {
            let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
            let actor = current_subject().unwrap_or_else(|| "metrics.cost_report".into());
            if let Err(e) = record_cost_report_activity(
                state.cfg.transport.data_dir.as_deref(),
                &tenant_id,
                &actor,
                &peer,
                hours,
                &v,
            ) {
                tracing::warn!(error = %e, "failed to append cost report activity");
            }
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => resp,
    }
}

// ── mesh helpers ─────────────────────────────────────────

fn record_cost_report_activity(
    data_dir: Option<&FsPath>,
    tenant_id: &str,
    actor: &str,
    peer: &str,
    hours: u32,
    report: &Value,
) -> Result<usize, String> {
    let Some(rows) = report.as_array() else {
        return Ok(0);
    };
    let mut appended = 0_usize;
    for row in rows {
        let Some(agent) = row.get("agent").and_then(Value::as_str) else {
            continue;
        };
        let Some(method) = row.get("method").and_then(Value::as_str) else {
            continue;
        };
        let Some(total_cost_micros) = value_as_i64(row.get("total_cost_micros")) else {
            continue;
        };
        let total_tokens = row.get("total_tokens").and_then(Value::as_u64).unwrap_or(0);
        let invocations = row.get("invocations").and_then(Value::as_u64).unwrap_or(0);
        if append_cost_report_activity(
            data_dir,
            CostReportActivity {
                tenant_id,
                actor,
                peer,
                hours,
                agent,
                method,
                total_cost_micros,
                total_tokens,
                invocations,
            },
        )? {
            appended = appended.saturating_add(1);
        }
    }
    Ok(appended)
}

fn value_as_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|v| {
        v.as_i64().or_else(|| {
            let n = v.as_u64()?;
            i64::try_from(n).ok()
        })
    })
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
            // UNKNOWN_METHOD means the coordinator did not register the
            // metrics capability — the agent-metrics feature is not
            // enabled on this deployment (default boot). These are all
            // read-only dashboard queries, so return a clean
            // "unavailable" marker (HTTP 200) instead of a 502; the
            // panels render empty. Admission is unchanged.
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

/// Clean "feature not enabled" body returned (HTTP 200) when the
/// responder reports UNKNOWN_METHOD for a read-only metrics call, so
/// the dashboard renders an empty panel rather than a 502 error box.
fn unavailable(method: &str) -> Value {
    serde_json::json!({
        "available": false,
        "reason": format!("capability '{method}' is not enabled on this deployment"),
    })
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
        let bytes = rt.block_on(async move { to_bytes(body, 64_000).await.unwrap() });
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            parsed.get("error").and_then(serde_json::Value::as_str),
            Some("agent is required")
        );
    }

    #[test]
    fn record_cost_report_activity_appends_paid_rows_once() {
        let tmp = tempfile::tempdir().unwrap();
        let report = serde_json::json!([
            {
                "agent": "alice",
                "method": "ai.chat",
                "total_cost_micros": 18_000,
                "total_tokens": 12_000,
                "invocations": 100
            },
            {
                "agent": "alice",
                "method": "tool.fs.read",
                "total_cost_micros": 0,
                "total_tokens": 0,
                "invocations": 5
            }
        ]);

        let first = record_cost_report_activity(
            Some(tmp.path()),
            "tenant-a",
            "operator-1",
            "coordinator",
            24,
            &report,
        )
        .unwrap();
        let second = record_cost_report_activity(
            Some(tmp.path()),
            "tenant-a",
            "operator-1",
            "coordinator",
            24,
            &report,
        )
        .unwrap();

        assert_eq!(first, 1);
        assert_eq!(second, 0);
        let body = std::fs::read_to_string(tmp.path().join("bridge-activity.jsonl")).unwrap();
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 1);
        let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry["source"], "cost");
        assert_eq!(entry["actor"], "operator-1");
        assert_eq!(entry["tenant_id"], "tenant-a");
        assert_eq!(entry["action"], "metrics.cost_report.observed");
        assert_eq!(entry["target"], "alice/ai.chat");
        assert_eq!(entry["cost_micros"], 18_000);
    }

    #[test]
    fn record_cost_report_activity_ignores_unavailable_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let report = serde_json::json!({
            "available": false,
            "reason": "metrics disabled"
        });

        let count = record_cost_report_activity(
            Some(tmp.path()),
            "tenant-a",
            "operator-1",
            "coordinator",
            24,
            &report,
        )
        .unwrap();

        assert_eq!(count, 0);
        assert!(!tmp.path().join("bridge-activity.jsonl").exists());
    }
}
