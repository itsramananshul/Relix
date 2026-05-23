//! HTTP proxies for the plugin_host node's management surface.
//!
//! Four endpoints — the bridge does not own plugins or know
//! about subprocess lifecycle; each handler calls the
//! configured plugin_host peer over the mesh and parses the
//! tab/pipe-delimited body.
//!
//! - `GET  /v1/plugins`              → `plugin.list`
//! - `GET  /v1/plugins/:id`          → `plugin.status`
//! - `POST /v1/plugins/:id/reload`   → `plugin.reload`
//! - `POST /v1/plugins/:id/disable`  → `plugin.disable`

use axum::{
    Json,
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "plugin_host";

#[derive(Debug, Deserialize, Default)]
pub struct PeerQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PluginRow {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub status: String,
    pub capabilities_count: usize,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub peer: String,
    pub plugins: Vec<PluginRow>,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub status: String,
    pub registered_at: i64,
    pub last_seen_at: Option<i64>,
    pub capabilities: Vec<String>,
    pub node_type: String,
    pub error_message: String,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<PeerQuery>,
) -> Result<Json<ListResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = call_peer_string(&state, &peer, "plugin.list", &[]).await?;
    let plugins = parse_list_body(&body);
    Ok(Json(ListResponse { peer, plugins }))
}

pub async fn status(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
    Query(q): Query<PeerQuery>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ApiError>)> {
    if plugin_id.trim().is_empty() {
        return Err(bad_request("plugin_id required".into()));
    }
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = call_peer_string(&state, &peer, "plugin.status", plugin_id.as_bytes()).await?;
    let parsed = parse_status_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!("plugin.status returned an unparseable body: {body:?}"),
        }),
    ))?;
    Ok(Json(parsed))
}

pub async fn reload(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
    Query(q): Query<PeerQuery>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    if plugin_id.trim().is_empty() {
        return Err(bad_request("plugin_id required".into()));
    }
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let _ = call_peer_string(&state, &peer, "plugin.reload", plugin_id.as_bytes()).await?;
    Ok(Json(OkResponse { ok: true }))
}

pub async fn disable(
    State(state): State<AppState>,
    AxumPath(plugin_id): AxumPath<String>,
    Query(q): Query<PeerQuery>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    if plugin_id.trim().is_empty() {
        return Err(bad_request("plugin_id required".into()));
    }
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let _ = call_peer_string(&state, &peer, "plugin.disable", plugin_id.as_bytes()).await?;
    Ok(Json(OkResponse { ok: true }))
}

pub fn parse_list_body(body: &str) -> Vec<PluginRow> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") {
            continue;
        }
        let cols: Vec<&str> = line.splitn(5, '\t').collect();
        if cols.len() != 5 {
            continue;
        }
        let Ok(caps_count) = cols[4].parse::<usize>() else {
            continue;
        };
        out.push(PluginRow {
            plugin_id: cols[0].to_string(),
            name: cols[1].to_string(),
            version: cols[2].to_string(),
            status: cols[3].to_string(),
            capabilities_count: caps_count,
        });
    }
    out
}

pub fn parse_status_body(body: &str) -> Option<StatusResponse> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut plugin_id = String::new();
    let mut name = String::new();
    let mut version = String::new();
    let mut status = String::new();
    let mut registered_at: i64 = 0;
    let mut last_seen_at: Option<i64> = None;
    let mut capabilities: Vec<String> = Vec::new();
    let mut node_type = String::new();
    let mut error_message = String::new();
    for kv in trimmed.split('|') {
        let (k, v) = kv.split_once('=')?;
        match k.trim() {
            "plugin_id" => plugin_id = v.to_string(),
            "name" => name = v.to_string(),
            "version" => version = v.to_string(),
            "status" => status = v.to_string(),
            "registered_at" => registered_at = v.trim().parse().ok()?,
            "last_seen_at" => {
                let n: i64 = v.trim().parse().ok()?;
                last_seen_at = if n < 0 { None } else { Some(n) };
            }
            "capabilities" if !v.is_empty() => {
                capabilities = v.split(',').map(|s| s.to_string()).collect();
            }
            "node_type" => node_type = v.to_string(),
            "error_message" => error_message = v.to_string(),
            _ => {}
        }
    }
    if plugin_id.is_empty() {
        return None;
    }
    Some(StatusResponse {
        plugin_id,
        name,
        version,
        status,
        registered_at,
        last_seen_at,
        capabilities,
        node_type,
        error_message,
    })
}

fn bad_request(msg: String) -> (StatusCode, Json<ApiError>) {
    (StatusCode::BAD_REQUEST, Json(ApiError { error: msg }))
}

async fn call_peer_string(
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
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 60);
    let envelope = build_request(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
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
            // Map plugin-not-found from the coordinator to 404
            // so the dashboard / CLI can branch on shape rather
            // than parse the error string.
            let status = if env.cause.contains("not found") {
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
                error: "unexpected stream response from plugin_host peer".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_typical_body() {
        let body = "abc123\thello-plugin\t0.1.0\tactive\t1\n\
                    def456\tweb-lookup\t0.2.0\terror\t2\n\
                    count=2\n";
        let v = parse_list_body(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].plugin_id, "abc123");
        assert_eq!(v[0].name, "hello-plugin");
        assert_eq!(v[0].version, "0.1.0");
        assert_eq!(v[0].status, "active");
        assert_eq!(v[0].capabilities_count, 1);
        assert_eq!(v[1].status, "error");
        assert_eq!(v[1].capabilities_count, 2);
    }

    #[test]
    fn parse_list_skips_count_line_only() {
        assert!(parse_list_body("count=0\n").is_empty());
    }

    #[test]
    fn parse_list_skips_malformed_rows() {
        let body = "abc\tone\ttwo\n\
                    abc\tfull\trow\tactive\t3\n\
                    count=1\n";
        let v = parse_list_body(body);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].plugin_id, "abc");
    }

    #[test]
    fn parse_status_typical_body() {
        let body = "plugin_id=abc123|name=hello-plugin|version=0.1.0|status=active|registered_at=1700000000|last_seen_at=1700000100|capabilities=hello.greet,hello.echo|node_type=|error_message=\n";
        let p = parse_status_body(body).unwrap();
        assert_eq!(p.plugin_id, "abc123");
        assert_eq!(p.name, "hello-plugin");
        assert_eq!(p.status, "active");
        assert_eq!(p.registered_at, 1700000000);
        assert_eq!(p.last_seen_at, Some(1700000100));
        assert_eq!(p.capabilities, vec!["hello.greet", "hello.echo"]);
        assert_eq!(p.error_message, "");
    }

    #[test]
    fn parse_status_empty_capabilities_treated_as_empty_vec() {
        let body = "plugin_id=abc|name=x|version=0.1.0|status=registered|registered_at=1|last_seen_at=-1|capabilities=|node_type=|error_message=\n";
        let p = parse_status_body(body).unwrap();
        assert!(p.capabilities.is_empty());
        assert_eq!(p.last_seen_at, None);
    }

    #[test]
    fn parse_status_empty_body_returns_none() {
        assert!(parse_status_body("").is_none());
        assert!(parse_status_body("   ").is_none());
    }
}
