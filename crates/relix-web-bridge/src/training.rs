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

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject, current_tenant};

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
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
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
        None,
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
    match call_peer_json(&state, &peer, "training.get_interaction", &body, None).await {
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
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
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
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = export_detail(&req);
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
    match call_peer_json(
        &state,
        &peer,
        "training.export",
        &Value::Object(body),
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_training_activity(
                &state,
                &peer,
                "training.export",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_training_activity(
                &state,
                &peer,
                "training.export",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
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
    let task_id = match clean_optional_id(q.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(q.run_id.as_deref());
    let detail = interaction_detail(&id);
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "interaction_id": id });
    match call_peer_json(
        &state,
        &peer,
        "training.score_interaction",
        &body,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_training_activity(
                &state,
                &peer,
                "training.score_interaction",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_training_activity(
                &state,
                &peer,
                "training.score_interaction",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

/// `GET /v1/training/stats`
pub async fn stats(
    State(state): State<AppState>,
    Query(q): Query<StatsQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "training.stats", &Value::Null, None).await {
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
    let task_id = match clean_optional_id(q.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(q.run_id.as_deref());
    let detail = interaction_detail(&id);
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "interaction_id": id });
    match call_peer_json(
        &state,
        &peer,
        "training.delete_interaction",
        &body,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_training_activity(
                &state,
                &peer,
                "training.delete_interaction",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_training_activity(
                &state,
                &peer,
                "training.delete_interaction",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
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
    match call_peer_json(&state, &peer, "training.pii_scan", &body, None).await {
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
        None,
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
    task_id: Option<&str>,
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
        task_id.map(str::to_string),
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

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn clean_optional_id(value: Option<&str>, field: &str) -> Result<Option<String>, String> {
    let Some(clean) = clean_optional(value) else {
        return Ok(None);
    };
    if clean.len() == 32 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(clean))
    } else {
        Err(format!("{field} must be 32 hex chars"))
    }
}

fn attach_scope(value: &mut Value, task_id: Option<&str>, run_id: Option<&str>) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if let Some(task_id) = task_id {
        obj.insert("task_id".into(), Value::String(task_id.to_string()));
    }
    if let Some(run_id) = run_id {
        obj.insert("run_id".into(), Value::String(run_id.to_string()));
    }
}

fn export_detail(req: &ExportRequest) -> String {
    format!(
        "format={}; export_set={}; agent={}; session_filter={}; max_interactions={}; output_dir_present={}",
        req.format.trim(),
        req.export_set.trim(),
        req.agent
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("all"),
        req.session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        req.max_interactions
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        req.output_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some()
    )
}

fn interaction_detail(interaction_id: &str) -> String {
    format!("interaction_id={}", interaction_id.trim())
}

fn record_training_activity(
    state: &AppState,
    peer: &str,
    method: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "training".into());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer,
            method,
            task_id,
            run_id,
            decision,
            detail,
        },
    ) {
        tracing::warn!(error = %e, method, "failed to append training activity");
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), task_id) {
        let payload = format!("peer={peer} outcome={decision} {detail}");
        let rec = rec.clone();
        let task_id = task_id.to_string();
        let event_type = method.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, &event_type, &payload).await;
        });
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

    #[test]
    fn export_request_accepts_scope_context_and_redacts_path_detail() {
        let req: ExportRequest = serde_json::from_str(
            r#"{
                "format":"openai",
                "export_set":"launch-eval",
                "output_dir":"C:/very/private/path",
                "agent":"alice",
                "session_id":"session-1",
                "max_interactions":25,
                "task_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id":"run-1"
            }"#,
        )
        .unwrap();
        assert_eq!(
            req.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(req.run_id.as_deref(), Some("run-1"));
        let detail = export_detail(&req);
        assert!(detail.contains("format=openai"));
        assert!(detail.contains("export_set=launch-eval"));
        assert!(detail.contains("agent=alice"));
        assert!(detail.contains("session_filter=true"));
        assert!(detail.contains("max_interactions=25"));
        assert!(detail.contains("output_dir_present=true"));
        assert!(!detail.contains("C:/very/private/path"));
        assert!(!detail.contains("session-1"));
    }

    #[test]
    fn peer_query_accepts_scope_context() {
        let q: PeerQuery = serde_json::from_str(
            r#"{"task_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","run_id":"run-2"}"#,
        )
        .unwrap();
        assert_eq!(
            q.task_id.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(q.run_id.as_deref(), Some("run-2"));
        assert_eq!(interaction_detail("abc123"), "interaction_id=abc123");
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut value = serde_json::json!({ "exported": true });
        attach_scope(
            &mut value,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some("run-1"),
        );
        assert_eq!(value["task_id"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(value["run_id"], "run-1");

        let mut scalar = serde_json::json!("ok");
        attach_scope(&mut scalar, Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), None);
        assert_eq!(scalar, serde_json::json!("ok"));
    }

    #[test]
    fn clean_optional_id_rejects_invalid_task_id() {
        assert_eq!(
            clean_optional_id(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "task_id")
                .unwrap()
                .as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(clean_optional_id(Some("bad"), "task_id").is_err());
        assert_eq!(clean_optional_id(Some(" "), "task_id").unwrap(), None);
    }
}
