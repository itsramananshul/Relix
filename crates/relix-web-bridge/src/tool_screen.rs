//! `POST /v1/tools/screen` — HTTP proxy for the `tool.screen`
//! cap (GAP 10 PART 3). Routes the request through the bridge's
//! mesh client to whichever peer hosts the tool node.

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject, current_tenant};

const DEFAULT_PEER: &str = "tool";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

type ErrorReply = (StatusCode, Json<ApiError>);

#[derive(Debug, Deserialize)]
pub struct ScreenBody {
    #[serde(default)]
    pub region: Option<Region>,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub async fn capture(
    State(state): State<AppState>,
    Json(req): Json<ScreenBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let task_id = match clean_optional_id(req.task_id.as_deref(), "task_id") {
        Ok(id) => id,
        Err(resp) => return resp.into_response(),
    };
    let run_id = clean_optional(req.run_id.as_deref());
    let mut body = serde_json::Map::new();
    if let Some(r) = &req.region {
        body.insert(
            "region".into(),
            serde_json::to_value(r).unwrap_or(Value::Null),
        );
    }
    match call_peer_json(
        &state,
        &peer,
        "tool.screen",
        &Value::Object(body),
        task_id.as_deref(),
    )
    .await
    {
        Ok(mut v) => {
            attach_scope(&mut v, task_id.as_deref(), run_id.as_deref());
            record_screen_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                region_detail(req.region.as_ref()).as_str(),
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_screen_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                region_detail(req.region.as_ref()).as_str(),
            );
            resp
        }
    }
}

fn record_screen_activity(
    state: &AppState,
    peer: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "tool.screen".into());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer,
            method: "tool.screen",
            task_id,
            run_id,
            decision,
            detail,
        },
    ) {
        tracing::warn!(error = %e, "failed to append screen activity");
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), task_id) {
        let payload = format!("peer={peer} outcome={decision} {detail}");
        let rec = rec.clone();
        let task_id = task_id.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, "tool.screen", &payload).await;
        });
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

fn region_detail(region: Option<&Region>) -> String {
    match region {
        Some(r) => format!("region={},{},{},{}", r.x, r.y, r.width, r.height),
        None => "region=full".into(),
    }
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn clean_optional_id(value: Option<&str>, field: &str) -> Result<Option<String>, ErrorReply> {
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
    let arg_bytes = serde_json::to_vec(args).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("encode args: {e}"),
            }),
        )
            .into_response()
    })?;
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
                error: "unexpected stream response from tool peer".into(),
            }),
        )
            .into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_body_accepts_task_and_run_context() {
        let req: ScreenBody = serde_json::from_str(
            r#"{
                "task_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id": "run-1",
                "region": { "x": 1, "y": 2, "width": 3, "height": 4 }
            }"#,
        )
        .unwrap();

        assert_eq!(
            req.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(req.run_id.as_deref(), Some("run-1"));
        assert_eq!(region_detail(req.region.as_ref()), "region=1,2,3,4");
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut value = serde_json::json!({ "png_base64": "abc" });
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
        let resp = clean_optional_id(Some("nope"), "task_id").unwrap_err();
        assert_eq!(resp.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            clean_optional_id(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "task_id")
                .unwrap()
                .as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(clean_optional_id(Some(" "), "task_id").unwrap(), None);
    }
}
