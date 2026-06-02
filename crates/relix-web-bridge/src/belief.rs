//! RELIX-7.29 PART 3 — HTTP proxies for the `belief.*` caps.
//!
//! Two endpoints:
//!
//! - `GET  /v1/belief/:session_id` → `belief.get`
//! - `POST /v1/belief/:session_id` with `{"action":"reset"}` → `belief.reset`
//!
//! `subject_id` is optional on both — defaults to the bridge
//! identity's subject. `peer` overrides the coordinator alias.

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
pub struct GetQuery {
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PostBody {
    /// Only `reset` is currently honoured.
    pub action: String,
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

/// `GET /v1/belief/:session_id`
pub async fn get(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(q): Query<GetQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if session_id.trim().is_empty() {
        return bad_request("session_id is required");
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let mut body = serde_json::Map::new();
    body.insert("session_id".into(), Value::from(session_id));
    if let Some(s) = q.subject_id.as_ref() {
        body.insert("subject_id".into(), Value::from(s.clone()));
    }
    match call_peer_json(&state, &peer, "belief.get", &Value::Object(body), None).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/belief/:session_id`
pub async fn post(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<PostBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if session_id.trim().is_empty() {
        return bad_request("session_id is required");
    }
    if !req.action.trim().eq_ignore_ascii_case("reset") {
        return bad_request("action must be \"reset\"");
    }
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(e) => return bad_request(&e),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let detail = belief_reset_detail(&session_id, req.subject_id.as_deref());
    let mut body = serde_json::Map::new();
    body.insert("session_id".into(), Value::from(session_id));
    if let Some(s) = req.subject_id.as_ref() {
        body.insert("subject_id".into(), Value::from(s.clone()));
    }
    match call_peer_json(
        &state,
        &peer,
        "belief.reset",
        &Value::Object(body),
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_belief_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_belief_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
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

fn belief_reset_detail(session_id: &str, subject_id: Option<&str>) -> String {
    format!(
        "session_id={}; subject_id={}",
        session_id.trim(),
        subject_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("default")
    )
}

fn record_belief_activity(
    state: &AppState,
    peer: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "belief".into());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer,
            method: "belief.reset",
            task_id,
            run_id,
            decision,
            detail,
        },
    ) {
        tracing::warn!(error = %e, "failed to append belief activity");
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), task_id) {
        let payload = format!("peer={peer} outcome={decision} {detail}");
        let rec = rec.clone();
        let task_id = task_id.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, "belief.reset", &payload).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_body_accepts_task_and_run_context() {
        let body: PostBody = serde_json::from_str(
            r#"{
                "action":"reset",
                "subject_id":"alice",
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
        assert_eq!(
            belief_reset_detail("sess-1", body.subject_id.as_deref()),
            "session_id=sess-1; subject_id=alice"
        );
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut value = serde_json::json!({ "reset": true });
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
