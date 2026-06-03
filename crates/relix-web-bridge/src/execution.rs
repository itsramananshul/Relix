//! GAP 11 + 12 — HTTP proxies for the `execution.*` capability
//! surface on the coordinator.
//!
//! - `POST /v1/execution/rollback`               → execution.rollback
//! - `GET  /v1/execution/transactions/{id}`      → execution.transaction_get
//! - `GET  /v1/execution/evidence`               → execution.evidence
//!   (GAP 12; proxied with `?action_id=` / `?actor_id=` /
//!   `?limit=` query params)

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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

#[derive(Debug, Deserialize)]
pub struct RollbackBody {
    pub transaction_id: String,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

pub async fn rollback(State(state): State<AppState>, Json(req): Json<RollbackBody>) -> Response {
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = rollback_detail(&req);
    let body = serde_json::json!({ "transaction_id": req.transaction_id });
    match call_peer_json(
        &state,
        &peer,
        "execution.rollback",
        &body,
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_execution_activity(
                &state,
                &peer,
                "execution.rollback",
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_execution_activity(
                &state,
                &peer,
                "execution.rollback",
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            resp
        }
    }
}

pub async fn transaction_get(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let peer = DEFAULT_PEER.to_string();
    let body = serde_json::json!({ "transaction_id": id });
    match call_peer_json(&state, &peer, "execution.transaction_get", &body, None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize)]
pub struct EvidenceQuery {
    #[serde(default)]
    pub action_id: Option<String>,
    #[serde(default)]
    pub actor_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub peer: Option<String>,
}

pub async fn evidence(State(state): State<AppState>, Query(q): Query<EvidenceQuery>) -> Response {
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    if let Some(a) = q.action_id {
        body.insert("action_id".into(), Value::from(a));
    }
    if let Some(a) = q.actor_id {
        body.insert("actor_id".into(), Value::from(a));
    }
    if let Some(l) = q.limit {
        body.insert("limit".into(), Value::from(l));
    }
    match call_peer_json(
        &state,
        &peer,
        "execution.evidence",
        &Value::Object(body),
        None,
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
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

fn rollback_detail(req: &RollbackBody) -> String {
    format!("transaction_id={}", req.transaction_id.trim())
}

fn record_execution_activity(
    state: &AppState,
    peer: &str,
    method: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "execution".into());
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
        tracing::warn!(error = %e, method, "failed to append execution activity");
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

fn bad_request(msg: &str) -> Response {
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
                error: "unexpected stream response from execution peer".into(),
            }),
        )
            .into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_body_round_trips() {
        let b: RollbackBody = serde_json::from_str(
            r#"{
                "transaction_id":"tx-1",
                "task_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id":"run-1"
            }"#,
        )
        .unwrap();
        assert_eq!(b.transaction_id, "tx-1");
        assert!(b.peer.is_none());
        assert_eq!(
            b.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(b.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn evidence_query_defaults_when_empty_json() {
        let q: EvidenceQuery = serde_json::from_str("{}").unwrap();
        assert!(q.action_id.is_none());
        assert!(q.actor_id.is_none());
        assert!(q.limit.is_none());
    }

    #[test]
    fn evidence_query_round_trips_known_fields() {
        let q: EvidenceQuery =
            serde_json::from_str(r#"{"action_id":"a1","actor_id":"alice","limit":5}"#).unwrap();
        assert_eq!(q.action_id.as_deref(), Some("a1"));
        assert_eq!(q.actor_id.as_deref(), Some("alice"));
        assert_eq!(q.limit, Some(5));
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut value = serde_json::json!({ "rolled_back": true });
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
        assert!(clean_optional_id(Some("not-a-task"), "task_id").is_err());
        assert_eq!(clean_optional_id(Some(" "), "task_id").unwrap(), None);
    }

    #[test]
    fn rollback_activity_detail_is_sanitized() {
        let req = RollbackBody {
            transaction_id: " tx-123 ".into(),
            peer: None,
            task_id: None,
            run_id: None,
        };
        assert_eq!(rollback_detail(&req), "transaction_id=tx-123");
    }
}
