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

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject};

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
    match call_peer_json(
        &state,
        &peer,
        "memory.skill_search",
        &Value::Object(body),
        None,
    )
    .await
    {
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
        None,
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
    match call_peer_json(&state, &peer, "memory.skill_get", &body, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/skills` — manually create a skill.
pub async fn create(State(state): State<AppState>, Json(mut body): Json<Value>) -> Response {
    let metadata = match extract_bridge_metadata(&mut body) {
        Ok(metadata) => metadata,
        Err(resp) => return resp.into_response(),
    };
    let detail = skill_detail("memory.skill_store", &body);
    match call_peer_json(
        &state,
        &metadata.peer,
        "memory.skill_store",
        &body,
        metadata.task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(
                &mut v,
                metadata.task_id.as_deref(),
                metadata.run_id.as_deref(),
            );
            record_skill_activity(
                &state,
                SkillActivity {
                    peer: &metadata.peer,
                    task_id: metadata.task_id.as_deref(),
                    run_id: metadata.run_id.as_deref(),
                    method: "memory.skill_store",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_skill_activity(
                &state,
                SkillActivity {
                    peer: &metadata.peer,
                    task_id: metadata.task_id.as_deref(),
                    run_id: metadata.run_id.as_deref(),
                    method: "memory.skill_store",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
}

/// `PATCH /v1/skills/:id` — update one skill.
pub async fn update(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(mut body): Json<Value>,
) -> Response {
    let metadata = match extract_bridge_metadata(&mut body) {
        Ok(metadata) => metadata,
        Err(resp) => return resp.into_response(),
    };
    // Inject the path id so callers don't have to send it twice.
    if let Some(obj) = body.as_object_mut() {
        obj.insert("id".into(), Value::from(id));
    } else {
        body = serde_json::json!({ "id": id });
    }
    let detail = skill_detail("memory.skill_update", &body);
    match call_peer_json(
        &state,
        &metadata.peer,
        "memory.skill_update",
        &body,
        metadata.task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(
                &mut v,
                metadata.task_id.as_deref(),
                metadata.run_id.as_deref(),
            );
            record_skill_activity(
                &state,
                SkillActivity {
                    peer: &metadata.peer,
                    task_id: metadata.task_id.as_deref(),
                    run_id: metadata.run_id.as_deref(),
                    method: "memory.skill_update",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_skill_activity(
                &state,
                SkillActivity {
                    peer: &metadata.peer,
                    task_id: metadata.task_id.as_deref(),
                    run_id: metadata.run_id.as_deref(),
                    method: "memory.skill_update",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct DeprecateBody {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

/// `POST /v1/skills/:id/deprecate` — flip status to deprecated.
pub async fn deprecate(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<DeprecateBody>>,
) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(task_id) => task_id,
        Err(resp) => return resp.into_response(),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut payload = serde_json::Map::new();
    payload.insert("id".into(), Value::from(id));
    if let Some(r) = req.reason.clone() {
        payload.insert("reason".into(), Value::from(r));
    }
    let detail = deprecate_detail(&payload);
    match call_peer_json(
        &state,
        &peer,
        "memory.skill_deprecate",
        &Value::Object(payload),
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_skill_activity(
                &state,
                SkillActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "memory.skill_deprecate",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_skill_activity(
                &state,
                SkillActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "memory.skill_deprecate",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
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

#[derive(Debug)]
struct BridgeMetadata {
    peer: String,
    task_id: Option<String>,
    run_id: Option<String>,
}

fn extract_bridge_metadata(
    req: &mut Value,
) -> Result<BridgeMetadata, (StatusCode, Json<ApiError>)> {
    let Some(map) = req.as_object_mut() else {
        return Ok(BridgeMetadata {
            peer: DEFAULT_PEER.to_string(),
            task_id: None,
            run_id: None,
        });
    };
    let peer = map
        .remove("peer")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_PEER.to_string());
    let task_id = clean_optional_id(
        map.remove("task_id").as_ref().and_then(Value::as_str),
        "task_id",
    )?;
    let run_id = clean_optional(map.remove("run_id").as_ref().and_then(Value::as_str));
    Ok(BridgeMetadata {
        peer,
        task_id,
        run_id,
    })
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn clean_optional_id(
    value: Option<&str>,
    field: &str,
) -> Result<Option<String>, (StatusCode, Json<ApiError>)> {
    let Some(clean) = clean_optional(value) else {
        return Ok(None);
    };
    if clean.len() == 32 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(clean))
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{field} must be 32 hex chars"),
            }),
        ))
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

fn skill_detail(method: &str, body: &Value) -> String {
    let keys = body.as_object().map(|m| m.len()).unwrap_or(0);
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    format!("method={method}; id={id}; name={name}; payload_keys={keys}")
}

fn deprecate_detail(body: &serde_json::Map<String, Value>) -> String {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let reason_len = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0);
    format!("method=memory.skill_deprecate; id={id}; reason_len={reason_len}")
}

struct SkillActivity<'a> {
    peer: &'a str,
    task_id: Option<&'a str>,
    run_id: Option<&'a str>,
    method: &'a str,
    decision: &'a str,
    detail: &'a str,
}

fn record_skill_activity(state: &AppState, activity: SkillActivity<'_>) {
    let tenant_id = crate::tenant::current_tenant_or_none()
        .as_deref()
        .unwrap_or(DEFAULT_TENANT)
        .to_string();
    let actor = current_subject().unwrap_or_else(|| activity.method.to_string());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer: activity.peer,
            method: activity.method,
            task_id: activity.task_id,
            run_id: activity.run_id,
            decision: activity.decision,
            detail: activity.detail,
        },
    ) {
        tracing::warn!(
            error = %e,
            method = activity.method,
            "failed to append skill activity"
        );
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), activity.task_id) {
        let payload = format!(
            "peer={} outcome={} {}",
            activity.peer, activity.decision, activity.detail
        );
        let rec = rec.clone();
        let task_id = task_id.to_string();
        let event_type = activity.method.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, &event_type, &payload).await;
        });
    }
}

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
    task_id: Option<&str>,
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
        assert!(b.task_id.is_none());
        assert!(b.run_id.is_none());
    }

    #[test]
    fn deprecate_body_parses_minimal_json() {
        let b: DeprecateBody = serde_json::from_str("{}").unwrap();
        assert!(b.reason.is_none());
        let b: DeprecateBody = serde_json::from_str(r#"{"reason":"old"}"#).unwrap();
        assert_eq!(b.reason.as_deref(), Some("old"));
    }

    #[test]
    fn deprecate_body_parses_scope_context() {
        let b: DeprecateBody = serde_json::from_str(
            r#"{
                "reason": "old",
                "peer": "ai-2",
                "task_id": "0123456789abcdef0123456789abcdef",
                "run_id": "run-1"
            }"#,
        )
        .unwrap();
        assert_eq!(
            b.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(b.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn bridge_metadata_is_stripped_from_skill_payload() {
        let mut v = serde_json::json!({
            "peer": "ai-2",
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1",
            "name": "Build Widget",
            "steps": ["one", "two"]
        });
        let metadata = extract_bridge_metadata(&mut v).unwrap();
        assert_eq!(metadata.peer, "ai-2");
        assert_eq!(
            metadata.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(metadata.run_id.as_deref(), Some("run-1"));
        assert!(v.get("peer").is_none());
        assert!(v.get("task_id").is_none());
        assert!(v.get("run_id").is_none());
        assert_eq!(v["name"], "Build Widget");
    }

    #[test]
    fn clean_optional_id_rejects_invalid_task_id() {
        assert!(clean_optional_id(None, "task_id").unwrap().is_none());
        assert_eq!(
            clean_optional_id(Some(" 0123456789abcdef0123456789abcdef "), "task_id")
                .unwrap()
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let err = clean_optional_id(Some("bad"), "task_id").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut obj = serde_json::json!({"id": "skill-1"});
        attach_scope(
            &mut obj,
            Some("0123456789abcdef0123456789abcdef"),
            Some("run-1"),
        );
        assert_eq!(obj["task_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(obj["run_id"], "run-1");

        let mut scalar = Value::String("ok".into());
        attach_scope(
            &mut scalar,
            Some("0123456789abcdef0123456789abcdef"),
            Some("run-1"),
        );
        assert!(scalar.as_str().is_some());
    }

    #[test]
    fn skill_activity_detail_does_not_copy_skill_body() {
        let secret = "never log this operational recipe";
        let body = serde_json::json!({
            "id": "skill-1",
            "name": "Deploy",
            "body": secret
        });
        let detail = skill_detail("memory.skill_update", &body);
        assert!(detail.contains("id=skill-1"));
        assert!(detail.contains("name=Deploy"));
        assert!(detail.contains("payload_keys=3"));
        assert!(!detail.contains(secret));
    }

    #[test]
    fn deprecate_detail_does_not_copy_reason_text() {
        let secret = "this reason might contain sensitive operator notes";
        let mut body = serde_json::Map::new();
        body.insert("id".into(), Value::String("skill-1".into()));
        body.insert("reason".into(), Value::String(secret.into()));
        let detail = deprecate_detail(&body);
        assert!(detail.contains("id=skill-1"));
        assert!(detail.contains("reason_len=50"));
        assert!(!detail.contains(secret));
    }
}
