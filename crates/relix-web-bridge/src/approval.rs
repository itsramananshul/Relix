//! HTTP proxies for the approval-delivery surface.
//!
//! - `GET /v1/approval/:id/delivery` → `approval.delivery_status`
//!   (RELIX-7.30 PART 1)
//! - `GET /v1/approval/failed-deliveries` →
//!   `approval.failed_deliveries` (PART 6)
//! - `GET /v1/approval/pending` → `approval.list_pending`
//!   (PART 5 — dashboard surface)
//! - `POST /v1/approval/:id/decision` →
//!   `approval.record_decision` (PART 5 — dashboard vote
//!   buttons + CLI / programmatic clients)
//! - `GET /v1/approval/:id` → `coord.approval.get`
//!   (DEFERRED C — agent-side polling + operator-facing CLI
//!   surface). Returns HTTP 404 when the coordinator-side cap
//!   responds with INVALID_ARGS / "not found".

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct DeliveryQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

/// `GET /v1/approval/:id/delivery`
pub async fn delivery_status(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Query(q): Query<DeliveryQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if approval_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "approval_id is required".into(),
            }),
        )
            .into_response();
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = serde_json::json!({ "approval_id": approval_id });
    match call_peer_json(&state, &peer, "approval.delivery_status", &body, true).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// PART 6 — `GET /v1/approval/failed-deliveries?limit=...&peer=...`
///
/// Lists the rows that landed in `delivery_failed` state on
/// the coordinator's delivery store, newest-first. `limit`
/// defaults to 50; the coordinator caps it at 500. Operators
/// use this to reconcile approvals whose channel send
/// returned an error (Telegram 5xx, Slack `not_in_channel`,
/// SMTP refused, …).
#[derive(Debug, Deserialize, Default)]
pub struct FailedDeliveriesQuery {
    /// Override the responder peer. Defaults to `coordinator`.
    #[serde(default)]
    pub peer: Option<String>,
    /// Max rows to return. Server-side clamp `[1, 500]`.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Handler for `GET /v1/approval/failed-deliveries`.
pub async fn failed_deliveries(
    State(state): State<AppState>,
    Query(q): Query<FailedDeliveriesQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = match q.limit {
        Some(l) => serde_json::json!({ "limit": l }),
        None => serde_json::json!({}),
    };
    match call_peer_json(&state, &peer, "approval.failed_deliveries", &body, true).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// PART 5 — `GET /v1/approval/pending?limit=...&peer=...`
///
/// Dashboard surface for "approvals waiting on me." Returns
/// every row in `pending` status on the coordinator's
/// delivery store, newest-first. `limit` defaults to 50; the
/// coordinator caps at 500. The dashboard UI polls this on a
/// short interval and renders one card per row with
/// approve / deny buttons that POST to
/// `/v1/approval/:id/decision`.
#[derive(Debug, Deserialize, Default)]
pub struct PendingListQuery {
    /// Override the responder peer. Defaults to `coordinator`.
    #[serde(default)]
    pub peer: Option<String>,
    /// Max rows to return. Server-side clamp `[1, 500]`.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Handler for `GET /v1/approval/pending`.
pub async fn pending_list(
    State(state): State<AppState>,
    Query(q): Query<PendingListQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = match q.limit {
        Some(l) => serde_json::json!({ "limit": l }),
        None => serde_json::json!({}),
    };
    match call_peer_json(&state, &peer, "approval.list_pending", &body, true).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// PART 5 — `POST /v1/approval/:id/decision`
///
/// Dashboard / CLI vote endpoint. Body is
/// `{"decision":"approved|rejected|expired", "note":"…"}`.
/// Forwards to `approval.record_decision` on the coordinator,
/// which atomically updates the store row and cancels any
/// in-flight escalation timer for the approval id (PART 7).
#[derive(Debug, Deserialize, Default)]
pub struct DecisionQuery {
    /// Override the responder peer. Defaults to `coordinator`.
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DecisionBody {
    /// Operator's decision. Coordinator-side validation enforces
    /// the `approved | rejected | expired` set.
    pub decision: String,
    /// Free-form operator note. Optional; persisted alongside
    /// the decision.
    #[serde(default)]
    pub note: Option<String>,
}

/// Handler for `POST /v1/approval/:id/decision`.
pub async fn record_decision(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Query(q): Query<DecisionQuery>,
    Json(body): Json<DecisionBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if approval_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "approval_id is required".into(),
            }),
        )
            .into_response();
    }
    if body.decision.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "decision is required".into(),
            }),
        )
            .into_response();
    }
    let approval_id = approval_id.trim().to_string();
    let decision = body.decision.trim().to_string();
    let note = body.note.clone();
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let args = serde_json::json!({
        "approval_id": &approval_id,
        "decision": &decision,
        "note": note.as_deref(),
    });
    match call_peer_json(&state, &peer, "approval.record_decision", &args, false).await {
        Ok(v) => {
            let tenant_id =
                crate::tenant::current_tenant_or_none().unwrap_or_else(|| "default".into());
            let task_id = activity_task_id(&v);
            let detail = activity_detail(note.as_deref(), &v);
            if let Err(e) = crate::activity::append_approval_activity(
                state.cfg.transport.data_dir.as_deref(),
                &tenant_id,
                "operator",
                &approval_id,
                &decision,
                task_id.as_deref(),
                detail,
            ) {
                tracing::warn!(
                    approval_id = approval_id,
                    decision = decision,
                    error = %e,
                    "approval decision accepted but activity ledger append failed"
                );
            }
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(resp) => resp,
    }
}

fn activity_task_id(value: &Value) -> Option<String> {
    value
        .get("task_id")
        .or_else(|| value.get("taskId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn activity_detail(note: Option<&str>, response: &Value) -> String {
    if let Some(note) = note.map(str::trim).filter(|s| !s.is_empty()) {
        return note.to_string();
    }
    response
        .get("status")
        .and_then(Value::as_str)
        .map(|status| format!("coordinator status: {status}"))
        .unwrap_or_else(|| "approval decision recorded".into())
}

/// DEFERRED C — `GET /v1/approval/:id`
///
/// Per-approval status read for operator tooling. Calls
/// `coord.approval.get` on the coordinator peer and returns the
/// full JSON row on success. The coordinator's INVALID_ARGS
/// "not found" cause maps to HTTP 404 so CLI / dashboard code
/// can distinguish "no such approval" from "real error" via
/// status code alone.
#[derive(Debug, Deserialize, Default)]
pub struct GetApprovalQuery {
    /// Override the responder peer. Defaults to `coordinator`.
    #[serde(default)]
    pub peer: Option<String>,
}

/// Handler for `GET /v1/approval/:id`.
pub async fn get_approval(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Query(q): Query<GetApprovalQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if approval_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "approval_id is required".into(),
            }),
        )
            .into_response();
    }
    let peer = q.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    // The coordinator cap takes raw `approval_id` bytes (not
    // JSON), so we call the binary helper instead of the JSON
    // helper for this method.
    match call_peer_raw_to_json(
        &state,
        &peer,
        "coord.approval.get",
        approval_id.as_bytes(),
        true,
    )
    .await
    {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(resp) => resp,
    }
}

/// DEFERRED C: variant of [`call_peer_json`] that sends raw
/// bytes (no JSON encode) and parses a JSON response. Used by
/// `coord.approval.get` whose wire arg is the raw approval id.
/// Also maps a coordinator INVALID_ARGS / "not found" cause to
/// HTTP 404 so CLI clients see distinct status codes for
/// missing vs malformed inputs.
async fn call_peer_raw_to_json(
    state: &AppState,
    alias: &str,
    method: &str,
    arg_bytes: &[u8],
    graceful_unknown_method: bool,
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
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 120);
    let envelope = build_request_with_tenant(
        method,
        arg_bytes.to_vec(),
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
            // Optional-feature capability not registered on this
            // deployment (default boot has no approval store). For the
            // read-only dashboard surface, return a clean "unavailable"
            // marker (HTTP 200) so the panel renders empty instead of a
            // 502. Admission is unchanged — the responder still refused.
            if graceful_unknown_method && env.kind == relix_core::types::error_kinds::UNKNOWN_METHOD
            {
                return Ok(unavailable(method));
            }
            // DEFERRED C: surface the cap's "not found" deny as
            // an HTTP 404 so the CLI / dashboard can switch on
            // status code without sniffing the cause text.
            let status = if env.kind == relix_core::types::error_kinds::INVALID_ARGS {
                if env.cause.contains("not found") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                }
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

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
    graceful_unknown_method: bool,
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
            if graceful_unknown_method && env.kind == relix_core::types::error_kinds::UNKNOWN_METHOD
            {
                return Ok(unavailable(method));
            }
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

/// Clean "feature not enabled" body returned (HTTP 200) when the
/// responder reports UNKNOWN_METHOD for a read-only approval call, so
/// the dashboard renders an empty panel rather than a 502 error box.
fn unavailable(method: &str) -> Value {
    serde_json::json!({
        "available": false,
        "reason": format!("capability '{method}' is not enabled on this deployment"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_task_id_accepts_snake_and_camel_case() {
        assert_eq!(
            activity_task_id(&serde_json::json!({
                "task_id": " aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa "
            }))
            .as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            activity_task_id(&serde_json::json!({
                "taskId": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }))
            .as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn activity_detail_prefers_operator_note() {
        assert_eq!(
            activity_detail(
                Some(" approved by owner "),
                &serde_json::json!({"status": "ok"})
            ),
            "approved by owner"
        );
        assert_eq!(
            activity_detail(Some(" "), &serde_json::json!({"status": "ok"})),
            "coordinator status: ok"
        );
    }
}
