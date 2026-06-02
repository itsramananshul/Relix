//! PH-BRIDGE-MCP — HTTP proxy for the MCP registry on a tool
//! node.
//!
//! Three endpoints:
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
//! - `POST /v1/mcp/invoke` (PH-BRIDGE-MCP-INVOKE) — proxies
//!   `tool.mcp.invoke`. Body JSON: `{peer?, server_id,
//!   tool_name, task_id?, run_id?, args}`. Response: `{peer,
//!   server_id, tool_name, task_id?, run_id?, result}` on
//!   success. Honest about D-009: the underlying
//!   runtime returns `RuntimeNotConnected` today, which the
//!   bridge surfaces as 502 Bad Gateway with the responder's
//!   cause string. The proxy itself is ready for the moment
//!   the stdio runtime wiring lands.
//!
//! Pure translation: the bridge dispatches via the existing
//! `MeshClient::call(alias, envelope)` path (same one
//! `TaskRecorder` uses for `task.*`) and parses the tab-delim
//! response into structured JSON for dashboard / HTTP-tool
//! consumption. No new auth surface, no new dispatch surface;
//! just a projection of the tool node's MCP registry +
//! invocation surface.
//!
//! Fail modes:
//! - Bridge mesh client not initialized → 503 ServiceUnavailable.
//! - Peer alias not in `peers.toml` → 404 NotFound (via the
//!   underlying `MeshClient::call` error message classification).
//! - Tool node doesn't have MCP configured → 502 BadGateway with
//!   the responder's INVALID_ARGS cause propagated.
//! - `server_id` empty on `/v1/mcp/tools` or
//!   `/v1/mcp/invoke` → 400 BadRequest.
//! - `tool_name` empty on `/v1/mcp/invoke` → 400 BadRequest.

use std::time::Instant;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{McpInvocationActivity, append_mcp_invocation_activity};
use crate::config::AppState;
use crate::mcp_audit::McpAuditEntry;
use crate::tenant::{DEFAULT_TENANT, current_subject, current_tenant};

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
    let body = call_peer(&state, &peer, "tool.mcp.list_servers", b"", None)
        .await
        .map_err(drop_kind)?;
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
    let body = call_peer(
        &state,
        &peer,
        "tool.mcp.list_tools",
        q.server_id.as_bytes(),
        None,
    )
    .await
    .map_err(drop_kind)?;
    let tools = parse_tools(&body);
    Ok(Json(ToolsResponse {
        peer,
        server_id: q.server_id,
        tools,
    }))
}

/// PH-BRIDGE-MCP-AUDIT: query for `GET /v1/mcp/audit`.
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    /// Maximum entries returned (snapshot is newest-first).
    /// Clamped to ring capacity. Default 100.
    #[serde(default)]
    pub max: Option<usize>,
}

/// PH-BRIDGE-MCP-AUDIT: response for `GET /v1/mcp/audit`.
#[derive(Debug, Serialize)]
pub struct AuditResponse {
    pub entries: Vec<McpAuditEntry>,
    /// Total entries currently held by the ring (may exceed
    /// `entries.len()` when `max` capped the snapshot).
    pub count: usize,
}

pub async fn audit(
    State(state): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Json<AuditResponse> {
    let max = q.max.unwrap_or(100).max(1);
    let entries = state.mcp_audit.snapshot_newest_first(max);
    let count = state.mcp_audit.len();
    Json(AuditResponse { entries, count })
}

/// PH-BRIDGE-MCP-INVOKE: request body for `POST /v1/mcp/invoke`.
#[derive(Debug, Deserialize)]
pub struct InvokeRequest {
    #[serde(default)]
    pub peer: Option<String>,
    pub server_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    /// Tool arguments forwarded verbatim. Typically a JSON
    /// string (matching the tool's declared `inputSchema`), but
    /// the bridge does not interpret it — it joins
    /// `<server_id>|<tool_name>|<args>` into the SIMP-016 wire
    /// shape and forwards.
    #[serde(default)]
    pub args: String,
}

/// PH-BRIDGE-MCP-INVOKE: response shape on success. On failure
/// the standard `ApiError` is returned with an appropriate
/// status code (see module doc for the error-classification
/// table).
#[derive(Debug, Serialize)]
pub struct InvokeResponse {
    pub peer: String,
    pub server_id: String,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Verbatim responder body. Today (D-009) the runtime is
    /// not wired so this path returns 502 BadGateway with a
    /// `RuntimeNotConnected` cause — but the response shape is
    /// ready for the moment the live runtime ships.
    pub result: String,
}

pub async fn invoke(
    State(state): State<AppState>,
    Json(req): Json<InvokeRequest>,
) -> Result<Json<InvokeResponse>, (StatusCode, Json<ApiError>)> {
    // PH-BRIDGE-MCP-AUDIT: argument-validation rejects don't
    // touch the mesh, so they aren't recorded in the ring. The
    // ring is for *dispatched* invocations — anything that
    // reached the tool peer (or tried to). Same posture the
    // intervention-audit ring uses.
    if req.server_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "server_id required".into(),
            }),
        ));
    }
    if req.tool_name.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "tool_name required".into(),
            }),
        ));
    }
    let task_id = clean_optional_id(req.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let wire_arg = build_invoke_arg(&req.server_id, &req.tool_name, &req.args);
    let args_len = req.args.len();
    let started = Instant::now();
    let dispatched = call_peer(
        &state,
        &peer,
        "tool.mcp.invoke",
        wire_arg.as_bytes(),
        task_id.as_deref(),
    )
    .await;
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let ts_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "mcp.invoke".into());
    match dispatched {
        Ok(result) => {
            state.mcp_audit.push(McpAuditEntry {
                ts_secs,
                peer_alias: peer.clone(),
                server_id: req.server_id.clone(),
                tool_name: req.tool_name.clone(),
                args_len,
                outcome: "ok".into(),
                error_kind: None,
                duration_ms,
            });
            record_mcp_activity(
                &state,
                McpActivityParts {
                    tenant_id: &tenant_id,
                    actor: &actor,
                    peer: &peer,
                    server_id: &req.server_id,
                    tool_name: &req.tool_name,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    decision: "ok",
                    args_len,
                    duration_ms,
                    error_kind: None,
                },
            )
            .await;
            Ok(Json(InvokeResponse {
                peer,
                server_id: req.server_id,
                tool_name: req.tool_name,
                task_id,
                run_id,
                result,
            }))
        }
        Err((status, kind, body)) => {
            state.mcp_audit.push(McpAuditEntry {
                ts_secs,
                peer_alias: peer.clone(),
                server_id: req.server_id.clone(),
                tool_name: req.tool_name.clone(),
                args_len,
                outcome: "err".into(),
                error_kind: Some(kind.clone()),
                duration_ms,
            });
            record_mcp_activity(
                &state,
                McpActivityParts {
                    tenant_id: &tenant_id,
                    actor: &actor,
                    peer: &peer,
                    server_id: &req.server_id,
                    tool_name: &req.tool_name,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    decision: "err",
                    args_len,
                    duration_ms,
                    error_kind: Some(&kind),
                },
            )
            .await;
            Err((status, body))
        }
    }
}

struct McpActivityParts<'a> {
    tenant_id: &'a str,
    actor: &'a str,
    peer: &'a str,
    server_id: &'a str,
    tool_name: &'a str,
    task_id: Option<&'a str>,
    run_id: Option<&'a str>,
    decision: &'a str,
    args_len: usize,
    duration_ms: u64,
    error_kind: Option<&'a str>,
}

async fn record_mcp_activity(state: &AppState, parts: McpActivityParts<'_>) {
    if let Err(e) = append_mcp_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        McpInvocationActivity {
            tenant_id: parts.tenant_id,
            actor: parts.actor,
            peer: parts.peer,
            server_id: parts.server_id,
            tool_name: parts.tool_name,
            task_id: parts.task_id,
            run_id: parts.run_id,
            decision: parts.decision,
            args_len: parts.args_len,
            duration_ms: parts.duration_ms,
            error_kind: parts.error_kind,
        },
    ) {
        tracing::warn!(error = %e, "failed to append MCP activity");
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), parts.task_id) {
        let payload = format!(
            "peer={} server_id={} tool_name={} outcome={} args_len={} duration_ms={} error_kind={}",
            parts.peer,
            parts.server_id,
            parts.tool_name,
            parts.decision,
            parts.args_len,
            parts.duration_ms,
            parts.error_kind.unwrap_or("")
        );
        rec.event(task_id, "mcp.invoke", &payload).await;
    }
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

/// PH-BRIDGE-MCP-INVOKE: build the SIMP-016 wire arg for
/// `tool.mcp.invoke`. Wire shape is `<server_id>|<tool_name>|<args>`;
/// the responder's parser uses `splitn(3, '|')` so `args` may
/// contain pipes itself without breaking the split.
fn build_invoke_arg(server_id: &str, tool_name: &str, args: &str) -> String {
    format!("{server_id}|{tool_name}|{args}")
}

/// PH-BRIDGE-MCP: invoke a capability on a tool peer via the
/// existing MeshClient and return its body as a UTF-8 string.
/// Classifies errors into HTTP status codes and surfaces a
/// short `error_kind` tag (alongside the user-facing message)
/// so the audit ring can record what went wrong without
/// regexing the message back apart.
///
/// Error kinds:
/// - `"mesh_unavailable"` — bridge mesh client wasn't built.
/// - `"unknown_alias"` — `peers.toml` doesn't know the alias.
/// - `"mesh_error"` — any other libp2p / transport failure.
/// - `"decode_error"` — response envelope failed to parse.
/// - `"response_utf8"` — Ok body wasn't valid UTF-8.
/// - `"unexpected_stream"` — responder streamed instead of replied.
/// - `"responder_<kind>"` — responder returned a structured
///   error envelope; `<kind>` is the envelope's `kind` field
///   (`"runtime_not_connected"`, `"invalid_args"`, etc.).
async fn call_peer(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
    task_id: Option<&str>,
) -> Result<String, (StatusCode, String, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "mesh_unavailable".to_string(),
        Json(ApiError {
            error: "bridge mesh client not initialized (peer discovery failed at startup)".into(),
        }),
    ))?;
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        state.cfg.transport.deadline_secs,
        None,
        None,
        task_id.map(str::to_string),
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = mesh.call(alias, envelope).await.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_ascii_lowercase();
        let (status, kind) = if lower.contains("unknown alias") || lower.contains("no peer") {
            (StatusCode::NOT_FOUND, "unknown_alias".to_string())
        } else {
            (StatusCode::BAD_GATEWAY, "mesh_error".to_string())
        };
        (status, kind, Json(ApiError { error: msg }))
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            "decode_error".to_string(),
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
    })?;
    match resp.res {
        ResponseResult::Ok(body) => String::from_utf8(body.to_vec()).map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                "response_utf8".to_string(),
                Json(ApiError {
                    error: format!("response body utf8: {e}"),
                }),
            )
        }),
        ResponseResult::Err(env) => {
            let kind = format!("responder_{}", env.kind);
            Err((
                StatusCode::BAD_GATEWAY,
                kind,
                Json(ApiError {
                    error: format!("responder err kind={} cause={}", env.kind, env.cause),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            "unexpected_stream".to_string(),
            Json(ApiError {
                error: "unexpected stream response from list capability".into(),
            }),
        )),
    }
}

/// PH-BRIDGE-MCP-AUDIT: drop the `error_kind` from a
/// [`call_peer`] error so handlers that don't audit (servers /
/// tools) keep their old `(StatusCode, Json<ApiError>)` shape.
fn drop_kind(err: (StatusCode, String, Json<ApiError>)) -> (StatusCode, Json<ApiError>) {
    (err.0, err.2)
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

    // ── PH-BRIDGE-MCP-INVOKE: wire arg builder ──────────────────────

    #[test]
    fn build_invoke_arg_three_pipes() {
        // Standard JSON args.
        let a = build_invoke_arg("mcp-srv", "search", r#"{"q":"rust"}"#);
        assert_eq!(a, r#"mcp-srv|search|{"q":"rust"}"#);
    }

    #[test]
    fn build_invoke_arg_args_may_contain_pipes() {
        // The responder uses splitn(3, '|') so args MAY contain
        // pipes — they end up in the third field intact. Verify
        // the builder doesn't escape or strip pipes from args.
        let a = build_invoke_arg("srv", "fetch", "a|b|c|d");
        assert_eq!(a, "srv|fetch|a|b|c|d");
    }

    #[test]
    fn build_invoke_arg_empty_args_ok() {
        // tool.mcp.invoke accepts empty args (the dispatch
        // parser checks server_id + tool_name non-empty;
        // args may be the empty string).
        let a = build_invoke_arg("srv", "noop", "");
        assert_eq!(a, "srv|noop|");
    }

    #[test]
    fn build_invoke_arg_preserves_whitespace_in_args() {
        // Whitespace inside args is meaningful (e.g. multi-line
        // JSON); the builder should not trim or normalize.
        let a = build_invoke_arg("srv", "tool", "  {  \"k\": 1  }  ");
        assert_eq!(a, "srv|tool|  {  \"k\": 1  }  ");
    }

    // ── PH-BRIDGE-MCP-AUDIT: error_kind classification ──────────────

    #[test]
    fn invoke_request_accepts_optional_task_and_run_context() {
        let req: InvokeRequest = serde_json::from_str(
            r##"{
                "server_id": "browser",
                "tool_name": "click",
                "task_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id": "run-1",
                "args": "{\"selector\":\"#ok\"}"
            }"##,
        )
        .unwrap();

        assert_eq!(
            req.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(req.run_id.as_deref(), Some("run-1"));
        assert_eq!(req.peer, None);
    }

    #[test]
    fn clean_optional_id_rejects_non_task_ids() {
        let err = clean_optional_id(Some("not-a-task"), "task_id").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
        assert_eq!(clean_optional_id(Some("   "), "task_id").unwrap(), None);
        assert_eq!(
            clean_optional_id(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "task_id")
                .unwrap()
                .as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn invoke_response_includes_scope_when_present() {
        let response = InvokeResponse {
            peer: "tool".into(),
            server_id: "browser".into(),
            tool_name: "click".into(),
            task_id: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            run_id: Some("run-1".into()),
            result: "ok".into(),
        };

        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["task_id"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(value["run_id"], "run-1");
    }

    #[test]
    fn invoke_response_omits_empty_scope() {
        let response = InvokeResponse {
            peer: "tool".into(),
            server_id: "browser".into(),
            tool_name: "click".into(),
            task_id: None,
            run_id: None,
            result: "ok".into(),
        };

        let value = serde_json::to_value(response).unwrap();
        assert!(value.get("task_id").is_none());
        assert!(value.get("run_id").is_none());
    }

    #[test]
    fn drop_kind_collapses_three_tuple_to_two() {
        // The two non-audit callers (servers / tools) use
        // `drop_kind` to discard the new middle field so their
        // `?` returns keep their old `(StatusCode, Json<ApiError>)`
        // shape. Verify the adapter actually drops the middle.
        let err = (
            StatusCode::BAD_GATEWAY,
            "mesh_error".to_string(),
            Json(ApiError {
                error: "boom".into(),
            }),
        );
        let (status, body) = drop_kind(err);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body.0.error, "boom");
    }
}
