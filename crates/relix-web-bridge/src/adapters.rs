//! HTTP proxy for the agent-adapter (**Rig**) registry.
//!
//! - `GET /v1/adapters` — proxies the coordinator's `rig.describe`
//!   capability. Returns the JSON array the runtime emits: one entry per
//!   registered adapter with its `name`, `display_name`, `governance`,
//!   `bridge_back`, `structured_output`, `billing`, and a **live
//!   availability probe** (`probe.status` = `available` / `missing`,
//!   `probe.detail`, `probe.install_hint`).
//!
//! This is what the dashboard Settings + Crew pages read to show which
//! local coding-agent CLIs (Claude, Codex, …) are actually installed —
//! never assuming either CLI exists. Read-only.

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const COORDINATOR: &str = "coordinator";

#[derive(serde::Serialize)]
pub struct ApiError {
    pub error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: msg.into() }))
}

/// `GET /v1/adapters` — the registered agent adapters + their live
/// availability probe.
pub async fn list(State(state): State<AppState>) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let body = call_coordinator_json(&state, "rig.describe", b"").await?;
    // `rig.describe` already emits a JSON array; pass it straight through.
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response())
}

/// Call a coordinator capability that returns a JSON body and hand back
/// the raw bytes. Mirrors the spine bridge's peer-call error mapping.
async fn call_coordinator_json(
    state: &AppState,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge mesh client not initialized",
        )
    })?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 60);
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = mesh.call(COORDINATOR, envelope).await.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_ascii_lowercase();
        let status = if lower.contains("unknown alias") || lower.contains("no peer") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_GATEWAY
        };
        err(status, msg)
    })?;
    let resp = decode_response(&resp_bytes)
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("decode response: {e}")))?;
    match resp.res {
        ResponseResult::Ok(body) => Ok(body.to_vec()),
        ResponseResult::Err(env) => {
            let lower = env.cause.to_ascii_lowercase();
            let status = if lower.contains("not found") {
                StatusCode::NOT_FOUND
            } else if env.kind == 5 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err(err(status, format!("responder err: {}", env.cause)))
        }
        ResponseResult::StreamHandle(_) => {
            Err(err(StatusCode::BAD_GATEWAY, "unexpected stream response"))
        }
    }
}
