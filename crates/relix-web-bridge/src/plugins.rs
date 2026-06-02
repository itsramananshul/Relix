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

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject, current_tenant};

const DEFAULT_PEER: &str = "plugin_host";

#[derive(Debug, Deserialize, Default)]
pub struct PeerQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
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
    let body = call_peer_string(&state, &peer, "plugin.list", &[], None).await?;
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
    let body = call_peer_string(&state, &peer, "plugin.status", plugin_id.as_bytes(), None).await?;
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
    let task_id = clean_optional_id(q.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(q.run_id.as_deref());
    match call_peer_string(
        &state,
        &peer,
        "plugin.reload",
        plugin_id.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(_) => {
            record_plugin_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "plugin.reload",
                "ok",
                &plugin_id,
            );
        }
        Err(err) => {
            record_plugin_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "plugin.reload",
                "err",
                &plugin_id,
            );
            return Err(err);
        }
    }
    Ok(Json(OkResponse {
        ok: true,
        task_id,
        run_id,
    }))
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
    let task_id = clean_optional_id(q.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(q.run_id.as_deref());
    match call_peer_string(
        &state,
        &peer,
        "plugin.disable",
        plugin_id.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(_) => {
            record_plugin_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "plugin.disable",
                "ok",
                &plugin_id,
            );
        }
        Err(err) => {
            record_plugin_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "plugin.disable",
                "err",
                &plugin_id,
            );
            return Err(err);
        }
    }
    Ok(Json(OkResponse {
        ok: true,
        task_id,
        run_id,
    }))
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

fn record_plugin_activity(
    state: &AppState,
    peer: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    method: &str,
    decision: &str,
    plugin_id: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| method.to_string());
    let detail = format!("plugin_id={plugin_id}");
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
            detail: &detail,
        },
    ) {
        tracing::warn!(error = %e, method, "failed to append plugin activity");
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

async fn call_peer_string(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
    task_id: Option<&str>,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized".into(),
        }),
    ))?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 60);
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
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

    #[test]
    fn peer_query_accepts_task_and_run_context() {
        let q: PeerQuery = serde_json::from_value(serde_json::json!({
            "peer": "plugin_host",
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1"
        }))
        .unwrap();
        assert_eq!(q.peer.as_deref(), Some("plugin_host"));
        assert_eq!(
            q.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(q.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn ok_response_omits_empty_scope_and_includes_present_scope() {
        let bare = serde_json::to_value(OkResponse {
            ok: true,
            task_id: None,
            run_id: None,
        })
        .unwrap();
        assert!(bare.get("task_id").is_none());
        assert!(bare.get("run_id").is_none());

        let scoped = serde_json::to_value(OkResponse {
            ok: true,
            task_id: Some("0123456789abcdef0123456789abcdef".into()),
            run_id: Some("run-2".into()),
        })
        .unwrap();
        assert_eq!(scoped["task_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(scoped["run_id"], "run-2");
    }

    #[test]
    fn clean_optional_id_rejects_invalid_task_id() {
        assert!(clean_optional_id(None, "task_id").unwrap().is_none());
        assert_eq!(
            clean_optional_id(Some(" 0123456789abcdef0123456789abcdef "), "task_id")
                .unwrap()
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let err = clean_optional_id(Some("bad"), "task_id").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
    }
}
