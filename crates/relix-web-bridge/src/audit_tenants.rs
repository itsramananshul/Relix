//! GAP 23C — HTTP proxies for per-tenant audit enumeration +
//! recent-row inspection.
//!
//! Two endpoints:
//!
//! - `GET /v1/audit/tenants?peer=<alias>` — proxies
//!   `node.audit.tenant_list`. Returns JSON
//!   `{"peer": ..., "tenants": [...], "count": N}`.
//! - `GET /v1/audit/tenants/:tenant_id?peer=<alias>&limit=N` —
//!   proxies `node.audit.tenant_recent`. Returns the raw JSON
//!   the responder produces (`{"tenant_id":..., "count":...,
//!   "rows": [...]}`). Limit defaults to 100, clamped to 1000.

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

const DEFAULT_PEER: &str = "tool";
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Deserialize)]
pub struct AuditTenantQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
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

/// `GET /v1/audit/tenants` — enumerate tenant ids known to the
/// peer's audit partition mirror. `count=0` means partitioning
/// is disabled or no traffic has flowed yet.
pub async fn list_tenants(
    State(state): State<AppState>,
    Query(q): Query<AuditTenantQuery>,
) -> Result<Json<TenantListResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer(&state, &peer, "node.audit.tenant_list", &[]).await?;
    let tenants = parse_tenant_list(&body);
    let count = tenants.len();
    Ok(Json(TenantListResponse {
        peer,
        tenants,
        count,
    }))
}

/// `GET /v1/audit/tenants/:tenant_id` — recent audit rows for a
/// tenant. Returns the JSON the responder produced verbatim
/// (the bridge does NOT re-shape the schema).
pub async fn recent(
    State(state): State<AppState>,
    Path(tenant_id): Path<String>,
    Query(q): Query<AuditTenantQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let arg = format!("{tenant_id}|{limit}");
    let body = call_peer(&state, &peer, "node.audit.tenant_recent", arg.as_bytes()).await?;
    let v: Value = serde_json::from_str(&body).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("response body not JSON: {e} (body={body:?})"),
            }),
        )
    })?;
    // Stamp the peer back onto the response so dashboards can
    // tag the source. We don't touch the responder's `rows`.
    let mut obj = v.as_object().cloned().unwrap_or_default();
    obj.insert("peer".into(), Value::String(peer));
    Ok(Json(Value::Object(obj)))
}

/// Parse the line-delim body emitted by
/// `controller_runtime::handle_audit_tenant_list`: one tenant
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
                error: "unexpected stream response from node.audit.tenant_*".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tenant_list_drops_count_and_blanks() {
        let body = "acme\nglobex\ndefault\ncount=3\n";
        assert_eq!(parse_tenant_list(body), vec!["acme", "globex", "default"]);
    }

    #[test]
    fn parse_tenant_list_empty_when_zero() {
        assert!(parse_tenant_list("count=0\n").is_empty());
        assert!(parse_tenant_list("").is_empty());
    }
}
