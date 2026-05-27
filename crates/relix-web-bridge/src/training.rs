//! RELIX-7.15 — HTTP proxies for the training data pipeline.
//!
//! Eight endpoints, each a thin forwarder onto a `training.*`
//! coordinator capability:
//!
//! - `GET    /v1/training/interactions`        — `training.list_interactions`
//! - `GET    /v1/training/interactions/:id`    — `training.get_interaction`
//! - `POST   /v1/training/export`              — `training.export`
//! - `POST   /v1/training/score/:id`           — `training.score_interaction`
//! - `GET    /v1/training/stats`               — `training.stats`
//! - `DELETE /v1/training/interactions/:id`    — `training.delete_interaction`
//! - `POST   /v1/training/pii/scan`            — `training.pii_scan`
//! - `POST   /v1/training/pii/preview`         — `training.anonymize_preview`
//!
//! Error mapping mirrors `/v1/metrics/*`:
//! - `INVALID_ARGS` → 400.
//! - peer alias missing → 404.
//! - `training: no interaction with id ...` (responder shape) → 404.
//! - responder fault → 502.
//! - mesh client not ready → 503.

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
pub struct ListQuery {
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, alias = "min_quality")]
    pub min_quality_score: Option<f32>,
    #[serde(default)]
    pub date_from: Option<i64>,
    #[serde(default)]
    pub date_to: Option<i64>,
    #[serde(default)]
    pub exported: Option<bool>,
    /// Override the coordinator peer alias (default
    /// `"coordinator"`).
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct StatsQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PeerQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

/// `GET /v1/training/interactions`
pub async fn list_interactions(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(v) = q.page {
        body.insert("page".into(), Value::from(v));
    }
    if let Some(v) = q.page_size {
        body.insert("page_size".into(), Value::from(v));
    }
    if let Some(v) = q.agent.clone() {
        body.insert("agent".into(), Value::from(v));
    }
    if let Some(v) = q.session_id.clone() {
        body.insert("session_id".into(), Value::from(v));
    }
    if let Some(v) = q.model.clone() {
        body.insert("model".into(), Value::from(v));
    }
    if let Some(v) = q.min_quality_score {
        body.insert("min_quality_score".into(), Value::from(v));
    }
    if let Some(v) = q.date_from {
        body.insert("date_from".into(), Value::from(v));
    }
    if let Some(v) = q.date_to {
        body.insert("date_to".into(), Value::from(v));
    }
    if let Some(v) = q.exported {
        body.insert("exported".into(), Value::from(v));
    }
    match call_peer_json(
        &state,
        &peer,
        "training.list_interactions",
        &Value::Object(body),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/training/interactions/:id`
pub async fn get_interaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if id.trim().is_empty() {
        return bad_request("interaction_id is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "interaction_id": id });
    match call_peer_json(&state, &peer, "training.get_interaction", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize)]
pub struct ExportRequest {
    pub format: String,
    pub export_set: String,
    #[serde(default)]
    pub output_dir: Option<String>,
    #[serde(default)]
    pub min_quality_score: Option<f32>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub date_from: Option<i64>,
    #[serde(default)]
    pub date_to: Option<i64>,
    #[serde(default)]
    pub max_interactions: Option<u32>,
    #[serde(default)]
    pub include_tool_calls: Option<bool>,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `POST /v1/training/export`
pub async fn export(
    State(state): State<AppState>,
    Json(req): Json<ExportRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.format.trim().is_empty() {
        return bad_request("format is required (openai / anthropic / generic / raw_json)");
    }
    if req.export_set.trim().is_empty() {
        return bad_request("export_set is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("format".into(), Value::from(req.format));
    body.insert("export_set".into(), Value::from(req.export_set));
    if let Some(v) = req.output_dir {
        body.insert("output_dir".into(), Value::from(v));
    }
    if let Some(v) = req.min_quality_score {
        body.insert("min_quality_score".into(), Value::from(v));
    }
    if let Some(v) = req.agent {
        body.insert("agent".into(), Value::from(v));
    }
    if let Some(v) = req.session_id {
        body.insert("session_id".into(), Value::from(v));
    }
    if let Some(v) = req.date_from {
        body.insert("date_from".into(), Value::from(v));
    }
    if let Some(v) = req.date_to {
        body.insert("date_to".into(), Value::from(v));
    }
    if let Some(v) = req.max_interactions {
        body.insert("max_interactions".into(), Value::from(v));
    }
    if let Some(v) = req.include_tool_calls {
        body.insert("include_tool_calls".into(), Value::from(v));
    }
    match call_peer_json(&state, &peer, "training.export", &Value::Object(body)).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/training/score/:id`
pub async fn score_interaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if id.trim().is_empty() {
        return bad_request("interaction_id is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "interaction_id": id });
    match call_peer_json(&state, &peer, "training.score_interaction", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/training/stats`
pub async fn stats(
    State(state): State<AppState>,
    Query(q): Query<StatsQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "training.stats", &Value::Null).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `DELETE /v1/training/interactions/:id`
pub async fn delete_interaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if id.trim().is_empty() {
        return bad_request("interaction_id is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "interaction_id": id });
    match call_peer_json(&state, &peer, "training.delete_interaction", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

// ── RELIX-7.15 PII endpoints ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PiiScanRequest {
    pub text: String,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `POST /v1/training/pii/scan`
pub async fn pii_scan(
    State(state): State<AppState>,
    Json(req): Json<PiiScanRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.text.is_empty() {
        return bad_request("text is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "text": req.text });
    match call_peer_json(&state, &peer, "training.pii_scan", &body).await {
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

/// `POST /v1/training/pii/preview`
pub async fn pii_preview(
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
        "training.anonymize_preview",
        &Value::Object(body),
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
            // Empty body → return `null` so callers don't choke
            // on JSON-parse. Otherwise parse the responder's
            // JSON.
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
            // The training capability surfaces "no interaction
            // with id ..." as RESPONDER_INTERNAL because the
            // dispatch layer doesn't carry a NOT_FOUND kind. We
            // sniff the cause string to map it to a 404 — same
            // pattern the metrics endpoints use for empty-window
            // queries.
            if env.kind == relix_core::types::error_kinds::INVALID_ARGS {
                Err((
                    StatusCode::BAD_REQUEST,
                    Json(ApiError {
                        error: format!("responder err kind=INVALID_ARGS cause={}", env.cause),
                    }),
                )
                    .into_response())
            } else if env
                .cause
                .to_ascii_lowercase()
                .contains("no interaction with id")
            {
                Err((
                    StatusCode::NOT_FOUND,
                    Json(ApiError {
                        error: env.cause.clone(),
                    }),
                )
                    .into_response())
            } else {
                Err((
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: format!("responder err kind={} cause={}", env.kind, env.cause),
                    }),
                )
                    .into_response())
            }
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
    fn bad_request_returns_400_with_error_body() {
        use axum::body::to_bytes;
        let resp = bad_request("interaction_id is required");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let bytes = rt.block_on(async { to_bytes(resp.into_body(), 64_000).await.unwrap() });
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            parsed.get("error").and_then(Value::as_str),
            Some("interaction_id is required")
        );
    }

    #[test]
    fn list_query_defaults_are_none() {
        let q = ListQuery::default();
        assert!(q.page.is_none());
        assert!(q.page_size.is_none());
        assert!(q.agent.is_none());
        assert!(q.peer.is_none());
    }
}
