//! RELIX-7.30 PART 2 — HTTP proxies for the `credentials.*`
//! caps.
//!
//! - `POST   /v1/credentials`              → `credentials.store`
//! - `GET    /v1/credentials`              → `credentials.list`
//! - `GET    /v1/credentials/:name`        → `credentials.get`
//! - `POST   /v1/credentials/:name/rotate` → `credentials.rotate`
//! - `POST   /v1/credentials/:name/revoke` → `credentials.revoke`
//! - `GET    /v1/credentials/:name/audit`  → `credentials.audit`

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
use crate::tenant::{DEFAULT_TENANT, current_subject};

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PeerQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub owner_agent: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StoreBody {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub owner_agent: Option<String>,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub rotation_interval_secs: Option<u64>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RotateBody {
    pub new_value: String,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevokeBody {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

pub async fn store(
    State(state): State<AppState>,
    Json(req): Json<StoreBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.name.trim().is_empty() || req.value.is_empty() {
        return bad_request("name and value are required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(task_id) => task_id,
        Err(resp) => return resp.into_response(),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let detail = store_detail(&req);
    let mut body = serde_json::Map::new();
    body.insert("name".into(), Value::from(req.name));
    body.insert("value".into(), Value::from(req.value));
    if let Some(k) = req.kind {
        body.insert("kind".into(), Value::from(k));
    }
    if let Some(o) = req.owner_agent {
        body.insert("owner_agent".into(), Value::from(o));
    }
    if let Some(e) = req.expires_at_ms {
        body.insert("expires_at_ms".into(), Value::from(e));
    }
    if let Some(r) = req.rotation_interval_secs {
        body.insert("rotation_interval_secs".into(), Value::from(r));
    }
    match call_peer_json(
        &state,
        &peer,
        "credentials.store",
        &Value::Object(body),
        false,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.store",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.store",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let task_id = match clean_optional_id(q.task_id.as_deref(), "task_id") {
        Ok(task_id) => task_id,
        Err(resp) => return resp.into_response(),
    };
    let run_id = clean_optional(q.run_id.as_deref());
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(o) = q.owner_agent {
        body.insert("owner_agent".into(), Value::from(o));
    }
    let detail = list_detail(&body);
    match call_peer_json(
        &state,
        &peer,
        "credentials.list",
        &Value::Object(body),
        true,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.list",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.list",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
}

pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if name.trim().is_empty() {
        return bad_request("name is required");
    }
    let task_id = match clean_optional_id(q.task_id.as_deref(), "task_id") {
        Ok(task_id) => task_id,
        Err(resp) => return resp.into_response(),
    };
    let run_id = clean_optional(q.run_id.as_deref());
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let detail = name_detail("credentials.get", &name);
    let body = serde_json::json!({ "name": name });
    match call_peer_json(
        &state,
        &peer,
        "credentials.get",
        &body,
        true,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.get",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.get",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
}

pub async fn rotate(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RotateBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if name.trim().is_empty() || req.new_value.is_empty() {
        return bad_request("name and new_value are required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(task_id) => task_id,
        Err(resp) => return resp.into_response(),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let detail = rotate_detail(&name, req.new_value.len());
    let body = serde_json::json!({ "name": name, "new_value": req.new_value });
    match call_peer_json(
        &state,
        &peer,
        "credentials.rotate",
        &body,
        false,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.rotate",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.rotate",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
}

pub async fn revoke(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RevokeBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if name.trim().is_empty() {
        return bad_request("name is required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(task_id) => task_id,
        Err(resp) => return resp.into_response(),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let detail = revoke_detail(&name, req.reason.as_deref());
    let mut body = serde_json::Map::new();
    body.insert("name".into(), Value::from(name));
    if let Some(r) = req.reason {
        body.insert("reason".into(), Value::from(r));
    }
    match call_peer_json(
        &state,
        &peer,
        "credentials.revoke",
        &Value::Object(body),
        false,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.revoke",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.revoke",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
}

pub async fn audit(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<PeerQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if name.trim().is_empty() {
        return bad_request("name is required");
    }
    let task_id = match clean_optional_id(q.task_id.as_deref(), "task_id") {
        Ok(task_id) => task_id,
        Err(resp) => return resp.into_response(),
    };
    let run_id = clean_optional(q.run_id.as_deref());
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("name".into(), Value::from(name));
    if let Some(l) = q.limit {
        body.insert("limit".into(), Value::from(l as u64));
    }
    let detail = audit_detail(&body);
    match call_peer_json(
        &state,
        &peer,
        "credentials.audit",
        &Value::Object(body),
        true,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.audit",
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_credential_activity(
                &state,
                CredentialActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "credentials.audit",
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
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

/// Clean "feature not enabled" body returned (HTTP 200) when the
/// responder reports UNKNOWN_METHOD for a read-only credential call, so
/// the dashboard renders an empty vault rather than a 502 error box.
fn unavailable(method: &str) -> Value {
    serde_json::json!({
        "available": false,
        "reason": format!("capability '{method}' is not enabled on this deployment"),
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

fn store_detail(req: &StoreBody) -> String {
    format!(
        "method=credentials.store; name={}; kind={}; owner_agent={}; value_len={}; expires_at_ms={}; rotation_interval_secs={}",
        req.name,
        req.kind.as_deref().unwrap_or(""),
        req.owner_agent.as_deref().unwrap_or(""),
        req.value.len(),
        req.expires_at_ms.map(|v| v.to_string()).unwrap_or_default(),
        req.rotation_interval_secs
            .map(|v| v.to_string())
            .unwrap_or_default()
    )
}

fn list_detail(body: &serde_json::Map<String, Value>) -> String {
    let owner_agent = body
        .get("owner_agent")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("method=credentials.list; owner_agent={owner_agent}")
}

fn name_detail(method: &str, name: &str) -> String {
    format!("method={method}; name={name}")
}

fn rotate_detail(name: &str, new_value_len: usize) -> String {
    format!("method=credentials.rotate; name={name}; new_value_len={new_value_len}")
}

fn revoke_detail(name: &str, reason: Option<&str>) -> String {
    let reason_len = reason.map(str::len).unwrap_or(0);
    format!("method=credentials.revoke; name={name}; reason_len={reason_len}")
}

fn audit_detail(body: &serde_json::Map<String, Value>) -> String {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    let limit = body.get("limit").and_then(Value::as_u64).unwrap_or(0);
    format!("method=credentials.audit; name={name}; limit={limit}")
}

struct CredentialActivity<'a> {
    peer: &'a str,
    task_id: Option<&'a str>,
    run_id: Option<&'a str>,
    method: &'a str,
    decision: &'a str,
    detail: &'a str,
}

fn record_credential_activity(state: &AppState, activity: CredentialActivity<'_>) {
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
            "failed to append credential activity"
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
    graceful_unknown_method: bool,
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
            // Credential vault not enabled on this deployment (default
            // boot has no [credentials] master key) → UNKNOWN_METHOD.
            // For read-only dashboard calls (list/get/audit) return a
            // clean "unavailable" marker (HTTP 200) so the panel shows
            // an empty vault instead of a 502. Mutating calls keep the
            // hard error. Admission is unchanged.
            if graceful_unknown_method && env.kind == relix_core::types::error_kinds::UNKNOWN_METHOD
            {
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
    fn store_body_accepts_task_and_run_context() {
        let req: StoreBody = serde_json::from_value(serde_json::json!({
            "name": "github",
            "value": "secret-token",
            "kind": "api_key",
            "owner_agent": "agent-1",
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1"
        }))
        .unwrap();
        assert_eq!(
            req.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(req.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn peer_query_accepts_task_and_run_context() {
        let q: PeerQuery = serde_json::from_value(serde_json::json!({
            "peer": "coordinator-2",
            "owner_agent": "agent-1",
            "limit": 5,
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1"
        }))
        .unwrap();
        assert_eq!(
            q.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(q.run_id.as_deref(), Some("run-1"));
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
        let mut obj = serde_json::json!({"ok": true});
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
        assert_eq!(scalar.as_str(), Some("ok"));
    }

    #[test]
    fn credential_store_detail_never_copies_secret_value() {
        let req = StoreBody {
            name: "github".into(),
            value: "do-not-log-this-token".into(),
            kind: Some("api_key".into()),
            owner_agent: Some("agent-1".into()),
            expires_at_ms: Some(123),
            rotation_interval_secs: Some(456),
            peer: None,
            task_id: None,
            run_id: None,
        };
        let detail = store_detail(&req);
        assert!(detail.contains("name=github"));
        assert!(detail.contains("kind=api_key"));
        assert!(detail.contains("value_len=21"));
        assert!(!detail.contains(&req.value));
    }

    #[test]
    fn credential_rotate_detail_never_copies_new_secret() {
        let secret = "new-secret-value";
        let detail = rotate_detail("github", secret.len());
        assert!(detail.contains("name=github"));
        assert!(detail.contains("new_value_len=16"));
        assert!(!detail.contains(secret));
    }

    #[test]
    fn credential_revoke_detail_never_copies_reason() {
        let reason = "operator wrote sensitive context here";
        let detail = revoke_detail("github", Some(reason));
        assert!(detail.contains("name=github"));
        assert!(detail.contains("reason_len=37"));
        assert!(!detail.contains(reason));
    }
}
