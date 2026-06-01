//! GAP 4 — HTTP proxies for `memory.skill_*` capabilities.
//!
//! Six endpoints, all thin forwarders to the matching coordinator
//! cap over the mesh:
//!
//! - `GET    /v1/skills`              → memory.skill_search
//! - `GET    /v1/skills/{id}`         → memory.skill_get
//! - `POST   /v1/skills`              → memory.skill_store
//! - `PATCH  /v1/skills/{id}`         → memory.skill_update
//! - `POST   /v1/skills/{id}/deprecate` → memory.skill_deprecate
//! - `GET    /v1/skills/stats`        → memory.skill_stats

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::{Json, response::IntoResponse, response::Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

// Skill capabilities (`memory.skill_*`) register on the AI node's
// dispatch bridge (nodes::ai::skill_caps::register), not the
// coordinator. Route there so the calls reach the node that serves
// them once `[skills]` is enabled.
const DEFAULT_PEER: &str = "ai";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub min_confidence: Option<f32>,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `GET /v1/skills` — search the skill catalogue.
pub async fn list(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> Response {
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("query".into(), Value::from(q.q.clone().unwrap_or_default()));
    if let Some(l) = q.limit {
        body.insert("limit".into(), Value::from(l));
    }
    if let Some(a) = q.agent.clone() {
        body.insert("agent".into(), Value::from(a));
    }
    if let Some(c) = q.min_confidence {
        body.insert("min_confidence".into(), Value::from(c));
    }
    match call_peer_json(&state, &peer, "memory.skill_search", &Value::Object(body)).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/skills/stats` — aggregate counts.
pub async fn stats(State(state): State<AppState>) -> Response {
    let peer = DEFAULT_PEER.to_string();
    match call_peer_json(
        &state,
        &peer,
        "memory.skill_stats",
        &Value::Object(Default::default()),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/skills/:id` — full skill detail with version history.
pub async fn get(State(state): State<AppState>, AxumPath(id): AxumPath<String>) -> Response {
    let peer = DEFAULT_PEER.to_string();
    let body = serde_json::json!({ "id": id });
    match call_peer_json(&state, &peer, "memory.skill_get", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/skills` — manually create a skill.
pub async fn create(State(state): State<AppState>, Json(mut body): Json<Value>) -> Response {
    let peer = body
        .as_object_mut()
        .and_then(|m| m.remove("peer"))
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_PEER.to_string());
    match call_peer_json(&state, &peer, "memory.skill_store", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `PATCH /v1/skills/:id` — update one skill.
pub async fn update(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(mut body): Json<Value>,
) -> Response {
    let peer = body
        .as_object_mut()
        .and_then(|m| m.remove("peer"))
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_PEER.to_string());
    // Inject the path id so callers don't have to send it twice.
    if let Some(obj) = body.as_object_mut() {
        obj.insert("id".into(), Value::from(id));
    } else {
        body = serde_json::json!({ "id": id });
    }
    match call_peer_json(&state, &peer, "memory.skill_update", &body).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct DeprecateBody {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

/// `POST /v1/skills/:id/deprecate` — flip status to deprecated.
pub async fn deprecate(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<DeprecateBody>>,
) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut payload = serde_json::Map::new();
    payload.insert("id".into(), Value::from(id));
    if let Some(r) = req.reason.clone() {
        payload.insert("reason".into(), Value::from(r));
    }
    match call_peer_json(
        &state,
        &peer,
        "memory.skill_deprecate",
        &Value::Object(payload),
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// Clean "feature not enabled" body (HTTP 200) when the responder
/// reports UNKNOWN_METHOD (e.g. `[skills]` disabled), so the panel
/// renders an empty state instead of a 502.
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
) -> Result<Value, Response> {
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
            if env.kind == relix_core::types::error_kinds::UNKNOWN_METHOD {
                return Ok(unavailable(method));
            }
            let status = if env.kind == relix_core::types::error_kinds::INVALID_ARGS {
                StatusCode::BAD_REQUEST
            } else if env.kind == relix_core::types::error_kinds::SECURITY_DENIED {
                StatusCode::FORBIDDEN
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
                error: "unexpected stream response from skill peer".into(),
            }),
        )
            .into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_serialises_with_error_field() {
        let body = serde_json::to_string(&ApiError {
            error: "boom".into(),
        })
        .unwrap();
        assert!(body.contains("\"error\":\"boom\""));
    }

    #[test]
    fn deprecate_body_defaults_to_empty() {
        let b = DeprecateBody::default();
        assert!(b.reason.is_none());
        assert!(b.peer.is_none());
    }

    #[test]
    fn deprecate_body_parses_minimal_json() {
        let b: DeprecateBody = serde_json::from_str("{}").unwrap();
        assert!(b.reason.is_none());
        let b: DeprecateBody = serde_json::from_str(r#"{"reason":"old"}"#).unwrap();
        assert_eq!(b.reason.as_deref(), Some("old"));
    }
}
