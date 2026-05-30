//! GAP 23B — HTTP proxies for per-tenant policy enumeration +
//! inspection.
//!
//! Two endpoints:
//!
//! - `GET /v1/policy/tenants?peer=<alias>` — proxies
//!   `node.policy.tenant_list`. Returns JSON `{"peer": ..., "tenants": [...], "count": N}`.
//! - `GET /v1/policy/tenants/:tenant_id?peer=<alias>` —
//!   proxies `node.policy.tenant_get`. Returns the raw TOML
//!   text under `{"peer": ..., "tenant_id": ..., "toml": ...}`.
//!   Maps the responder's `UNKNOWN_METHOD` "no such tenant"
//!   error to a 404 so dashboards can branch on status.
//!
//! Both are read-only proxies; the bridge does not parse the
//! tenant TOML itself — the responder is the source of truth.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "tool";

#[derive(Debug, Deserialize)]
pub struct PolicyTenantQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct TenantListResponse {
    pub peer: String,
    pub tenants: Vec<String>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct TenantGetResponse {
    pub peer: String,
    pub tenant_id: String,
    pub toml: String,
}

/// `GET /v1/policy/tenants` — list tenants known to the peer's
/// resolver. `count=0` means either no [policy] dir is
/// configured on the peer or the directory is empty.
pub async fn list_tenants(
    State(state): State<AppState>,
    Query(q): Query<PolicyTenantQuery>,
) -> Result<Json<TenantListResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer(&state, &peer, "node.policy.tenant_list", &[]).await?;
    let tenants = parse_tenant_list(&body);
    let count = tenants.len();
    Ok(Json(TenantListResponse {
        peer,
        tenants,
        count,
    }))
}

/// `GET /v1/policy/tenants/:tenant_id` — read the raw policy
/// TOML for `tenant_id` from the peer. 404 on miss.
pub async fn get_tenant(
    State(state): State<AppState>,
    Path(tenant_id): Path<String>,
    Query(q): Query<PolicyTenantQuery>,
) -> Result<Json<TenantGetResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer_typed(
        &state,
        &peer,
        "node.policy.tenant_get",
        tenant_id.as_bytes(),
    )
    .await?;
    Ok(Json(TenantGetResponse {
        peer,
        tenant_id,
        toml: body,
    }))
}

/// Parse the tab-/newline-delim body emitted by
/// `controller_runtime::handle_policy_tenant_list`: one tenant
/// id per line, trailing `count=N`.
fn parse_tenant_list(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let s = line.trim();
        if s.is_empty() || s.starts_with("count=") {
            continue;
        }
        out.push(s.to_string());
    }
    out
}

async fn call_peer(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<String, (StatusCode, Json<ApiError>)> {
    call_peer_typed(state, alias, method, arg).await
}

async fn call_peer_typed(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized".into(),
        }),
    ))?;
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        state.cfg.transport.deadline_secs,
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
        (status, Json(ApiError { error: msg }))
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
    })?;
    match resp.res {
        ResponseResult::Ok(body) => String::from_utf8(body.to_vec()).map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: format!("response body utf8: {e}"),
                }),
            )
        }),
        ResponseResult::Err(env) => {
            // Map the responder's UNKNOWN_METHOD "no such tenant"
            // signal to a 404 so dashboards can branch cleanly.
            let status = if env.kind == relix_core::types::error_kinds::UNKNOWN_METHOD {
                StatusCode::NOT_FOUND
            } else if env.kind == relix_core::types::error_kinds::INVALID_ARGS {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={}", env.kind, env.cause),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from node.policy.tenant_*".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tenant_list_drops_count_and_blanks() {
        let body = "acme\nglobex\nstark\ncount=3\n";
        let tenants = parse_tenant_list(body);
        assert_eq!(tenants, vec!["acme", "globex", "stark"]);
    }

    #[test]
    fn parse_tenant_list_empty_when_zero() {
        assert!(parse_tenant_list("count=0\n").is_empty());
        assert!(parse_tenant_list("").is_empty());
    }
}
