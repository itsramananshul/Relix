//! PH-BRIDGE-MCP — HTTP proxy for the MCP registry on a tool
//! node.
//!
//! Two endpoints:
//!
//! - `GET /v1/mcp/servers?peer=<alias>` — proxies
//!   `tool.mcp.list_servers` to the named tool peer (default
//!   alias `"tool"`). Returns JSON `{peer, servers:[{id,
//!   transport, endpoint, declared_tool_count, status}]}`.
//!
//! - `GET /v1/mcp/tools?peer=<alias>&server_id=<id>` — proxies
//!   `tool.mcp.list_tools`. Returns JSON `{peer, server_id,
//!   tools:[...]}`.
//!
//! Pure translation: the bridge dispatches via the existing
//! `MeshClient::call(alias, envelope)` path (same one
//! `TaskRecorder` uses for `task.*`) and parses the tab-delim
//! response into structured JSON for dashboard / HTTP-tool
//! consumption. No new auth surface, no new dispatch surface;
//! just a read-only projection of the tool node's MCP registry.
//!
//! Fail modes:
//! - Bridge mesh client not initialized → 503 ServiceUnavailable.
//! - Peer alias not in `peers.toml` → 404 NotFound (via the
//!   underlying `MeshClient::call` error message classification).
//! - Tool node doesn't have MCP configured → 502 BadGateway with
//!   the responder's INVALID_ARGS cause propagated.
//! - `server_id` empty on `/v1/mcp/tools` → 400 BadRequest.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

/// Default peer alias when the caller doesn't supply `?peer=`.
/// Matches the `peers.toml` convention for the tool node.
const DEFAULT_PEER: &str = "tool";

#[derive(Debug, Deserialize)]
pub struct ServersQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct McpServerRow {
    pub id: String,
    pub transport: String,
    pub endpoint: String,
    pub declared_tool_count: usize,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct ServersResponse {
    pub peer: String,
    pub servers: Vec<McpServerRow>,
}

#[derive(Debug, Deserialize)]
pub struct ToolsQuery {
    #[serde(default)]
    pub peer: Option<String>,
    pub server_id: String,
}

#[derive(Debug, Serialize)]
pub struct ToolsResponse {
    pub peer: String,
    pub server_id: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn servers(
    State(state): State<AppState>,
    Query(q): Query<ServersQuery>,
) -> Result<Json<ServersResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer(&state, &peer, "tool.mcp.list_servers", b"").await?;
    let servers = parse_servers(&body);
    Ok(Json(ServersResponse { peer, servers }))
}

pub async fn tools(
    State(state): State<AppState>,
    Query(q): Query<ToolsQuery>,
) -> Result<Json<ToolsResponse>, (StatusCode, Json<ApiError>)> {
    if q.server_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "server_id required".into(),
            }),
        ));
    }
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer(&state, &peer, "tool.mcp.list_tools", q.server_id.as_bytes()).await?;
    let tools = parse_tools(&body);
    Ok(Json(ToolsResponse {
        peer,
        server_id: q.server_id,
        tools,
    }))
}

/// PH-BRIDGE-MCP: invoke a capability on a tool peer via the
/// existing MeshClient and return its body as a UTF-8 string.
/// Classifies errors into HTTP status codes.
async fn call_peer(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized (peer discovery failed at startup)".into(),
        }),
    ))?;
    let envelope = build_request(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        state.cfg.transport.deadline_secs,
    );
    let resp_bytes = mesh.call(alias, envelope).await.map_err(|e| {
        let msg = e.to_string();
        // MeshClient's error messages contain "unknown alias" or
        // "no peer" when peers.toml doesn't have the alias.
        // Classify those as 404 so curl gets a meaningful code.
        let status = if msg.to_ascii_lowercase().contains("unknown alias")
            || msg.to_ascii_lowercase().contains("no peer")
        {
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
        ResponseResult::Err(env) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("responder err kind={} cause={}", env.kind, env.cause),
            }),
        )),
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from list capability".into(),
            }),
        )),
    }
}

/// PH-BRIDGE-MCP: parse tool.mcp.list_servers body
/// (`id\ttransport\tendpoint\tdeclared_tool_count\tstatus`, then
/// `count=N`) into structured rows. Drops malformed lines.
fn parse_servers(body: &str) -> Vec<McpServerRow> {
    let mut rows = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 5 {
            continue;
        }
        let declared_tool_count = parts[3].parse::<usize>().unwrap_or(0);
        rows.push(McpServerRow {
            id: parts[0].to_string(),
            transport: parts[1].to_string(),
            endpoint: parts[2].to_string(),
            declared_tool_count,
            status: parts[4].to_string(),
        });
    }
    rows
}

/// PH-BRIDGE-MCP: parse tool.mcp.list_tools body (one tool name
/// per line, then `count=N`). Returns just the names.
fn parse_tools(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| !l.starts_with("count=") && !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_servers_two_rows_with_count_trailer() {
        let body = "alpha\tstdio\tmcp-server\t5\tconfigured\n\
                    beta\thttp\thttps://example.com\t0\tconfigured\n\
                    count=2\n";
        let rows = parse_servers(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "alpha");
        assert_eq!(rows[0].transport, "stdio");
        assert_eq!(rows[0].declared_tool_count, 5);
        assert_eq!(rows[1].id, "beta");
        assert_eq!(rows[1].endpoint, "https://example.com");
    }

    #[test]
    fn parse_servers_handles_unparseable_tool_count_as_zero() {
        // declared_tool_count is parsed loosely — non-numeric
        // values yield 0, NOT a parse error (the bridge stays
        // up; the operator sees zero and investigates).
        let body = "alpha\tstdio\tmcp-server\tnot-a-number\tconfigured\ncount=1\n";
        let rows = parse_servers(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].declared_tool_count, 0);
    }

    #[test]
    fn parse_servers_skips_count_and_blanks() {
        let body = "\ncount=0\n";
        assert!(parse_servers(body).is_empty());
    }

    #[test]
    fn parse_servers_drops_rows_missing_columns() {
        let body = "broken\tstdio\tonly-three\ncount=0\n";
        assert!(parse_servers(body).is_empty());
    }

    #[test]
    fn parse_tools_returns_names_only() {
        let body = "search\nfetch\nclick\ncount=3\n";
        assert_eq!(parse_tools(body), vec!["search", "fetch", "click"]);
    }

    #[test]
    fn parse_tools_skips_count_and_blanks() {
        let body = "\nsearch\n\ncount=1\n";
        assert_eq!(parse_tools(body), vec!["search"]);
    }

    #[test]
    fn parse_tools_empty_body_returns_empty_vec() {
        assert!(parse_tools("").is_empty());
        assert!(parse_tools("count=0").is_empty());
    }
}
