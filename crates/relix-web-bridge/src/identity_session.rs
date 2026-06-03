//! RELIX-7.30 PART 3 — HTTP proxies for the session-identity
//! `identity.*` caps.
//!
//! - `POST /v1/identity/tokens`        → `identity.issue_token`
//! - `POST /v1/identity/tokens/verify` → `identity.verify_token`
//! - `POST /v1/identity/tokens/revoke` → `identity.revoke_token`
//! - `GET  /v1/identity/tokens`        → `identity.active_tokens`
//!
//! RELIX-7.18 / GAP 17 PART 2 — research-backed identity:
//!
//! - `POST /v1/identity/research`      → `identity.research`

use axum::{
    Json,
    extract::{Query, State},
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
    pub peer: Option<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IssueBody {
    pub session_id: String,
    pub agent_name: String,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VerifyBody {
    pub token: String,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevokeBody {
    pub session_id: String,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResearchBody {
    pub subject_name: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

pub async fn issue(
    State(state): State<AppState>,
    Json(req): Json<IssueBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.session_id.trim().is_empty() || req.agent_name.trim().is_empty() {
        return bad_request("session_id and agent_name are required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = issue_detail(&req);
    // Phase 7 — the token's tenant binding must come from the verified
    // principal, not the request body, so a caller can't mint a token
    // for another tenant.
    let tenant_id = match reconcile_issue_tenant(req.tenant_id.as_deref()) {
        Ok(t) => t,
        Err(e) => return forbidden(&e),
    };
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("session_id".into(), Value::from(req.session_id));
    body.insert("agent_name".into(), Value::from(req.agent_name));
    if let Some(t) = tenant_id {
        body.insert("tenant_id".into(), Value::from(t));
    }
    body.insert("scopes".into(), Value::from(req.scopes));
    if let Some(ttl) = req.ttl_secs {
        body.insert("ttl_secs".into(), Value::from(ttl));
    }
    match call_peer_json(
        &state,
        &peer,
        "identity.issue_token",
        &Value::Object(body),
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_identity_activity(
                &state,
                &peer,
                "identity.issue_token",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_identity_activity(
                &state,
                &peer,
                "identity.issue_token",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

pub async fn verify(
    State(state): State<AppState>,
    Json(req): Json<VerifyBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.token.trim().is_empty() {
        return bad_request("token is required");
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "token": req.token });
    match call_peer_json(&state, &peer, "identity.verify_token", &body, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

pub async fn revoke(
    State(state): State<AppState>,
    Json(req): Json<RevokeBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.session_id.trim().is_empty() {
        return bad_request("session_id is required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = revoke_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "session_id": req.session_id });
    match call_peer_json(
        &state,
        &peer,
        "identity.revoke_token",
        &body,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_identity_activity(
                &state,
                &peer,
                "identity.revoke_token",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_identity_activity(
                &state,
                &peer,
                "identity.revoke_token",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

pub async fn research(
    State(state): State<AppState>,
    Json(req): Json<ResearchBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.subject_name.trim().is_empty() {
        return bad_request("subject_name is required");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = research_detail(&req);
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("subject_name".into(), Value::from(req.subject_name));
    if let Some(c) = req.context {
        body.insert("context".into(), Value::from(c));
    }
    // The pipeline's approval gate can wait up to 5 minutes;
    // give the mesh call a 600s envelope so a slow operator
    // doesn't cap the synthesis before the gate finishes.
    match call_peer_json_with_deadline(
        &state,
        &peer,
        "identity.research",
        &Value::Object(body),
        600_i64,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_identity_activity(
                &state,
                &peer,
                "identity.research",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_identity_activity(
                &state,
                &peer,
                "identity.research",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(a) = q.agent_name {
        body.insert("agent_name".into(), Value::from(a));
    }
    match call_peer_json(
        &state,
        &peer,
        "identity.active_tokens",
        &Value::Object(body),
        None,
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
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

fn forbidden(msg: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::FORBIDDEN,
        Json(ApiError {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

/// Phase 7 — reconcile a caller-supplied `tenant_id` override on an
/// identity-token issue against the verified per-request tenant.
///
/// The issued token's tenant binding is a tenant-owned privilege, so a
/// caller must never mint a token for another tenant by setting
/// `tenant_id` in the request body. In multi-tenant mode the middleware
/// resolved a real verified tenant, so:
///   - a body claim that DISAGREES with it is a cross-tenant
///     escalation attempt → `Err` (surfaced as HTTP 403)
///   - the verified tenant is forced regardless of an omitted or
///     matching body claim
///
/// In single-tenant mode (no verified tenant binding) the body value
/// passes through unchanged so operators can still seed named tenants.
fn reconcile_issue_tenant(body_claim: Option<&str>) -> Result<Option<String>, String> {
    let claim = clean_optional(body_claim);
    match crate::tenant::current_tenant_or_none() {
        Some(verified) => {
            if let Some(c) = claim.as_deref()
                && c != verified
            {
                return Err(format!(
                    "tenant_id override {c:?} does not match the authenticated tenant"
                ));
            }
            Ok(Some(verified))
        }
        None => Ok(claim),
    }
}

/// Clean "feature not enabled" body (HTTP 200) when the responder
/// reports UNKNOWN_METHOD (session-identity caps not registered), so
/// the panel renders an empty state instead of a 502.
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
    task_id: Option<&str>,
) -> Result<Value, axum::response::Response> {
    let deadline = state.cfg.transport.deadline_secs.clamp(5, 120);
    call_peer_json_with_deadline(state, alias, method, args, deadline, task_id).await
}

async fn call_peer_json_with_deadline(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
    deadline_secs: i64,
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

fn issue_detail(req: &IssueBody) -> String {
    format!(
        "session_id={}; agent_name={}; tenant_override={}; scopes_count={}; ttl_secs={}",
        req.session_id.trim(),
        req.agent_name.trim(),
        req.tenant_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("none"),
        req.scopes.len(),
        req.ttl_secs
            .map(|ttl| ttl.to_string())
            .unwrap_or_else(|| "default".into())
    )
}

fn revoke_detail(req: &RevokeBody) -> String {
    format!("session_id={}", req.session_id.trim())
}

fn research_detail(req: &ResearchBody) -> String {
    format!(
        "subject_name_len={}; context_len={}",
        req.subject_name.len(),
        req.context.as_deref().map(str::len).unwrap_or(0)
    )
}

fn record_identity_activity(
    state: &AppState,
    peer: &str,
    method: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "identity".into());
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
        tracing::warn!(error = %e, method, "failed to append identity activity");
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
    fn issue_body_accepts_scope_context() {
        let body: IssueBody = serde_json::from_str(
            r#"{
                "session_id":"sess-1",
                "agent_name":"alice",
                "scopes":["ai.chat"],
                "task_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id":"run-1"
            }"#,
        )
        .unwrap();
        assert_eq!(
            body.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(body.run_id.as_deref(), Some("run-1"));
        assert!(issue_detail(&body).contains("scopes_count=1"));
    }

    #[test]
    fn research_detail_does_not_copy_subject_or_context() {
        let body = ResearchBody {
            subject_name: "private subject".into(),
            context: Some("sensitive context".into()),
            peer: None,
            task_id: None,
            run_id: None,
        };
        let detail = research_detail(&body);
        assert!(detail.contains("subject_name_len=15"));
        assert!(detail.contains("context_len=17"));
        assert!(!detail.contains("private subject"));
        assert!(!detail.contains("sensitive context"));
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut value = serde_json::json!({ "token": "secret" });
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

    // Phase 7 — the issued token's tenant binding must come from the
    // verified principal, never the request body.
    #[tokio::test]
    async fn issue_tenant_forces_verified_and_rejects_mismatch() {
        // Multi-tenant: verified tenant-a, body omits → forced to a.
        let forced = crate::tenant::CURRENT_TENANT
            .scope("tenant-a".to_string(), async {
                reconcile_issue_tenant(None)
            })
            .await;
        assert_eq!(forced, Ok(Some("tenant-a".to_string())));

        // Multi-tenant: verified tenant-a, body agrees → ok.
        let agree = crate::tenant::CURRENT_TENANT
            .scope("tenant-a".to_string(), async {
                reconcile_issue_tenant(Some("tenant-a"))
            })
            .await;
        assert_eq!(agree, Ok(Some("tenant-a".to_string())));

        // Multi-tenant: verified tenant-a, body claims tenant-b → reject.
        let spoof = crate::tenant::CURRENT_TENANT
            .scope("tenant-a".to_string(), async {
                reconcile_issue_tenant(Some("tenant-b"))
            })
            .await;
        assert!(spoof.is_err());
    }

    #[tokio::test]
    async fn issue_tenant_single_tenant_mode_passes_body_through() {
        // Single-tenant sentinel → current_tenant_or_none() is None,
        // so the body override (if any) passes through for seeding.
        let passthrough = crate::tenant::CURRENT_TENANT
            .scope(DEFAULT_TENANT.to_string(), async {
                reconcile_issue_tenant(Some("seed-tenant"))
            })
            .await;
        assert_eq!(passthrough, Ok(Some("seed-tenant".to_string())));

        let none = crate::tenant::CURRENT_TENANT
            .scope(DEFAULT_TENANT.to_string(), async {
                reconcile_issue_tenant(None)
            })
            .await;
        assert_eq!(none, Ok(None));
    }
}
