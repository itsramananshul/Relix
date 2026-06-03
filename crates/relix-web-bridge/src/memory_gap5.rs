//! GAP 5 — HTTP proxies for the four missing memory caps.
//!
//! All four endpoints proxy a single JSON object straight through
//! to the configured memory peer over the mesh:
//!
//! - `POST /v1/memory/dialectic`       → `memory.dialectic`.
//! - `POST /v1/memory/ingest`          → `memory.ingest_document`.
//! - `POST /v1/memory/ingest_image`    → `memory.ingest_image`.
//! - `POST /v1/memory/context_flush`   → `memory.context_flush`.
//!
//! The bridge does not own a `LayeredMemoryStore` writer — it
//! always rides the mesh so the memory controller stays the
//! single writer. When `mesh_client` is unset, every handler
//! responds 503 with a structured body.

use axum::extract::Extension;
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, TenantId, current_subject};

const DEFAULT_PEER: &str = "memory";

#[derive(Debug, serde::Serialize)]
pub struct ApiError {
    pub error: String,
}

/// `POST /v1/memory/dialectic` — forwards the entire request body
/// (less the optional `peer` key) to `memory.dialectic`.
pub async fn dialectic(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, Some(tenant.0.as_str()), req, "memory.dialectic").await
}

/// `POST /v1/memory/ingest` — forwards to `memory.ingest_document`.
pub async fn ingest(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(
        &state,
        Some(tenant.0.as_str()),
        req,
        "memory.ingest_document",
    )
    .await
}

/// `POST /v1/memory/ingest_image` — forwards to `memory.ingest_image`.
pub async fn ingest_image(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, Some(tenant.0.as_str()), req, "memory.ingest_image").await
}

/// `POST /v1/memory/context_flush` — forwards to `memory.context_flush`.
pub async fn context_flush(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, Some(tenant.0.as_str()), req, "memory.context_flush").await
}

/// `POST /v1/memory/quarantine/list` — forwards to `memory.quarantine_list`.
pub async fn quarantine_list(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(
        &state,
        Some(tenant.0.as_str()),
        req,
        "memory.quarantine_list",
    )
    .await
}

/// `POST /v1/memory/quarantine/approve` — forwards to `memory.quarantine_approve`.
pub async fn quarantine_approve(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(
        &state,
        Some(tenant.0.as_str()),
        req,
        "memory.quarantine_approve",
    )
    .await
}

/// `POST /v1/memory/quarantine/reject` — forwards to `memory.quarantine_reject`.
pub async fn quarantine_reject(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(
        &state,
        Some(tenant.0.as_str()),
        req,
        "memory.quarantine_reject",
    )
    .await
}

// ── GAP 7: inspector editing surface ──────────────────────

/// `POST /v1/memory/records/edit` — forwards to `memory.edit_record`.
pub async fn edit_record(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, Some(tenant.0.as_str()), req, "memory.edit_record").await
}

/// `POST /v1/memory/records/freeze` — forwards to `memory.freeze_record`.
pub async fn freeze_record(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, Some(tenant.0.as_str()), req, "memory.freeze_record").await
}

/// `POST /v1/memory/records/unfreeze` — forwards to `memory.unfreeze_record`.
pub async fn unfreeze_record(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(
        &state,
        Some(tenant.0.as_str()),
        req,
        "memory.unfreeze_record",
    )
    .await
}

/// `POST /v1/memory/export` — forwards to `memory.bulk_export`.
pub async fn bulk_export(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(&state, Some(tenant.0.as_str()), req, "memory.bulk_export").await
}

/// `POST /v1/memory/refresh_model` — forwards to `memory.request_model_refresh`.
pub async fn request_model_refresh(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<Value>,
) -> axum::response::Response {
    proxy_json(
        &state,
        Some(tenant.0.as_str()),
        req,
        "memory.request_model_refresh",
    )
    .await
}

// ── helpers ──────────────────────────────────────────────

async fn proxy_json(
    state: &AppState,
    tenant: Option<&str>,
    mut req: Value,
    method: &str,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let metadata = match extract_bridge_metadata(&mut req) {
        Ok(metadata) => metadata,
        Err(resp) => return resp.into_response(),
    };
    let detail = memory_detail(method, &req);
    match call_peer_json(
        state,
        &metadata.peer,
        tenant,
        method,
        &req,
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
            record_memory_activity(
                state,
                MemoryActivity {
                    tenant,
                    peer: &metadata.peer,
                    task_id: metadata.task_id.as_deref(),
                    run_id: metadata.run_id.as_deref(),
                    method,
                    decision: "ok",
                    detail: &detail,
                },
            );
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => {
            record_memory_activity(
                state,
                MemoryActivity {
                    tenant,
                    peer: &metadata.peer,
                    task_id: metadata.task_id.as_deref(),
                    run_id: metadata.run_id.as_deref(),
                    method,
                    decision: "err",
                    detail: &detail,
                },
            );
            resp
        }
    }
}

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    tenant: Option<&str>,
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
    // Document + image ingestion can sit on a vision model for a
    // while — give the deadline a wider ceiling than the default
    // bridge calls so the bridge does not time out before the
    // memory peer has a chance to answer.
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(15, 600);
    // GAP 23: stamp the tenant onto the envelope so the
    // memory peer's per-cap handlers can scope Qdrant /
    // policy / audit per tenant.
    let envelope = build_request_with_tenant(
        method,
        arg_bytes,
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        task_id.map(str::to_string),
        tenant.map(str::to_string),
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
                error: "unexpected stream response from memory peer".into(),
            }),
        )
            .into_response()),
    }
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

fn should_record_activity(method: &str) -> bool {
    matches!(
        method,
        "memory.ingest_document"
            | "memory.ingest_image"
            | "memory.context_flush"
            | "memory.quarantine_approve"
            | "memory.quarantine_reject"
            | "memory.edit_record"
            | "memory.freeze_record"
            | "memory.unfreeze_record"
            | "memory.request_model_refresh"
            | "memory.bulk_export"
    )
}

fn memory_detail(method: &str, req: &Value) -> String {
    let keys = req.as_object().map(|m| m.len()).unwrap_or(0);
    let subject = req
        .get("subject_id")
        .and_then(Value::as_str)
        .or_else(|| req.get("record_id").and_then(Value::as_str))
        .unwrap_or("");
    format!("method={method}; subject_or_record={subject}; payload_keys={keys}")
}

struct MemoryActivity<'a> {
    tenant: Option<&'a str>,
    peer: &'a str,
    task_id: Option<&'a str>,
    run_id: Option<&'a str>,
    method: &'a str,
    decision: &'a str,
    detail: &'a str,
}

fn record_memory_activity(state: &AppState, activity: MemoryActivity<'_>) {
    if !should_record_activity(activity.method) {
        return;
    }
    let tenant_id = activity.tenant.unwrap_or(DEFAULT_TENANT);
    let actor = current_subject().unwrap_or_else(|| activity.method.to_string());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id,
            actor: &actor,
            peer: activity.peer,
            method: activity.method,
            task_id: activity.task_id,
            run_id: activity.run_id,
            decision: activity.decision,
            detail: activity.detail,
        },
    ) {
        tracing::warn!(error = %e, method = activity.method, "failed to append memory activity");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_override_is_stripped_from_payload() {
        // Sanity: when the caller sends `peer` in the body, the
        // helper removes it so the cap doesn't see an extra field
        // (caps reject unknown keys).
        let mut v = serde_json::json!({
            "peer": "memory-2",
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1",
            "observer_id": "agent.alpha",
            "subject_id": "user.bob",
            "question": "what color is the sky"
        });
        let metadata = extract_bridge_metadata(&mut v).unwrap();
        assert_eq!(metadata.peer, "memory-2");
        assert_eq!(
            metadata.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(metadata.run_id.as_deref(), Some("run-1"));
        assert!(v.get("peer").is_none());
        assert!(v.get("task_id").is_none());
        assert!(v.get("run_id").is_none());
        assert_eq!(
            v.get("observer_id").and_then(Value::as_str),
            Some("agent.alpha")
        );
    }

    #[test]
    fn metadata_extraction_rejects_invalid_task_id() {
        let mut v = serde_json::json!({
            "task_id": "not-a-task",
            "subject_id": "user.bob"
        });
        let err = extract_bridge_metadata(&mut v).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
    }

    #[test]
    fn attach_scope_only_mutates_object_responses() {
        let mut obj = serde_json::json!({ "ok": true });
        attach_scope(
            &mut obj,
            Some("0123456789abcdef0123456789abcdef"),
            Some("run-1"),
        );
        assert_eq!(obj["task_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(obj["run_id"], "run-1");

        let mut scalar = serde_json::json!("ok");
        attach_scope(
            &mut scalar,
            Some("0123456789abcdef0123456789abcdef"),
            Some("run-1"),
        );
        assert_eq!(scalar, serde_json::json!("ok"));
    }

    #[test]
    fn activity_filter_records_only_memory_write_methods() {
        assert!(should_record_activity("memory.ingest_document"));
        assert!(should_record_activity("memory.edit_record"));
        assert!(should_record_activity("memory.bulk_export"));
        assert!(!should_record_activity("memory.dialectic"));
        assert!(!should_record_activity("memory.quarantine_list"));
    }

    #[test]
    fn memory_detail_does_not_copy_payload_text() {
        let req = serde_json::json!({
            "subject_id": "user.bob",
            "document": "secret document body",
            "metadata": { "source": "manual" }
        });
        let detail = memory_detail("memory.ingest_document", &req);
        assert_eq!(
            detail,
            "method=memory.ingest_document; subject_or_record=user.bob; payload_keys=3"
        );
        assert!(!detail.contains("secret document body"));
    }
}
