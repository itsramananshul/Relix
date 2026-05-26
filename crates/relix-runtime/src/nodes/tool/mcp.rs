//! CW5 — MCP (Model Context Protocol) registry + runtime.
//!
//! Hermes ships `mcp_tool` which auto-discovers tools exposed
//! by external MCP servers (stdio or HTTP transport) and
//! projects them into the agent's capability catalog. Relix's
//! CW5 foundation lands the **registry + discovery model**;
//! PH-MCP-RUNTIME (D-009 closed) layers a live stdio client on
//! top — `tool.mcp.invoke` against an `stdio` server now
//! spawns the operator-declared subprocess, runs the MCP
//! `initialize` handshake, and dispatches `tools/call`. HTTP
//! transport still returns `RuntimeNotConnected` until the
//! HTTP client ships.
//!
//! ## Honesty contract
//!
//! Per the operator directive:
//! *"If actual MCP execution requires a later runtime decision,
//!  build the registry/discovery model first and label execution
//!  as not connected yet. No fake MCP execution."*
//!
//! Concrete posture:
//!
//! - `[[tool.mcp.servers]]` config entries register servers.
//!   Each entry has `id`, `transport` (`"stdio"` | `"http"`),
//!   `endpoint` (legacy program name OR base URL),
//!   `command` + `args` (stdio-only, PH-MCP-RUNTIME),
//!   `declared_tools`, and `description`.
//! - `tool.mcp.list_servers` returns the operator-declared
//!   server list with `status = "configured"`. The dashboard
//!   can grow a richer `connected` projection when live
//!   health checks ship.
//! - `tool.mcp.list_tools|<server_id>` runs a live `tools/list`
//!   when transport=stdio; on transport failure falls back to
//!   the operator-declared list (never fabricated). Non-stdio
//!   transports return the declared list.
//! - `tool.mcp.invoke|<server_id>|<tool_name>|<args>` runs the
//!   live stdio dispatch when transport=stdio. Spawn / I/O
//!   failures surface as `RuntimeNotConnected` with the cause
//!   prefixed `mcp:`. Malformed responses surface as
//!   `RESPONDER_INTERNAL` with `mcp: bad response: ...`. HTTP
//!   transport still returns `RuntimeNotConnected`.
//!
//! Operators reading the chronicle / audit will never see a
//! fake MCP tool invocation. Per-D-002 the trust-tier
//! decision is still operator-facing and won't be silently
//! resolved.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};

use super::mcp_http::{McpHttpClient, map_http_err};
use super::mcp_stdio::{McpStdioClient, StdioError, encode_tools_call_result};
use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

// ─────────────────────────── PH-MCP-PROTO: JSON-RPC wire layer ─────────────
//
// Pure data layer for the Model Context Protocol (MCP) JSON-RPC
// envelopes. No I/O lives here — that lands in a follow-up
// milestone (PH-MCP-STDIO1) once an operator identifies a real
// MCP server target (decisions-pending D-009). Shipping the
// protocol layer ahead of the runtime keeps the next session
// from having to bootstrap both the wire shape and the
// connection management in the same change.
//
// MCP protocol version targeted: 2024-11-05. Spec reference:
// https://spec.modelcontextprotocol.io/specification/

pub mod proto {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    /// JSON-RPC 2.0 protocol literal.
    pub const JSONRPC_VERSION: &str = "2.0";

    /// MCP protocol version Relix targets. Sent in the
    /// `initialize` request; servers respond with their own
    /// version which the client must reconcile.
    pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

    /// JSON-RPC 2.0 request (id required, response expected).
    #[derive(Debug, Clone, Serialize)]
    pub struct JsonRpcRequest {
        pub jsonrpc: String,
        pub id: u64,
        pub method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub params: Option<Value>,
    }

    /// JSON-RPC 2.0 response (id echoed back; either result or
    /// error is set, never both). The `serde(default)` posture
    /// tolerates servers that emit only one field.
    #[derive(Debug, Clone, Deserialize)]
    pub struct JsonRpcResponse {
        pub jsonrpc: String,
        pub id: u64,
        #[serde(default)]
        pub result: Option<Value>,
        #[serde(default)]
        pub error: Option<JsonRpcError>,
    }

    /// JSON-RPC 2.0 error object.
    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub struct JsonRpcError {
        pub code: i32,
        pub message: String,
        #[serde(default)]
        pub data: Option<Value>,
    }

    /// JSON-RPC 2.0 notification (no id, no response expected).
    /// Used for `notifications/initialized` and progress events.
    #[derive(Debug, Clone, Serialize)]
    pub struct JsonRpcNotification {
        pub jsonrpc: String,
        pub method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub params: Option<Value>,
    }

    impl JsonRpcRequest {
        /// Generic constructor — most callers prefer one of the
        /// purpose-built constructors below.
        pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
            Self {
                jsonrpc: JSONRPC_VERSION.into(),
                id,
                method: method.into(),
                params,
            }
        }

        /// Build the `initialize` request that opens an MCP
        /// session. The server replies with its own
        /// `protocolVersion`, `capabilities`, and `serverInfo`.
        pub fn initialize(id: u64, client_name: &str, client_version: &str) -> Self {
            Self::new(
                id,
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": client_name,
                        "version": client_version,
                    },
                })),
            )
        }

        /// Build the `tools/list` request. Result body is a
        /// [`ToolsListResult`].
        pub fn tools_list(id: u64) -> Self {
            Self::new(id, "tools/list", None)
        }

        /// Build the `tools/call` request. `args` is the tool
        /// input — typically a JSON object matching the tool's
        /// declared `inputSchema`.
        pub fn tools_call(id: u64, name: &str, args: Value) -> Self {
            Self::new(
                id,
                "tools/call",
                Some(serde_json::json!({
                    "name": name,
                    "arguments": args,
                })),
            )
        }
    }

    impl JsonRpcNotification {
        pub fn new(method: impl Into<String>) -> Self {
            Self {
                jsonrpc: JSONRPC_VERSION.into(),
                method: method.into(),
                params: None,
            }
        }

        /// Sent by the client immediately after a successful
        /// `initialize` response. Tells the server the client
        /// is ready to send normal requests.
        pub fn initialized() -> Self {
            Self::new("notifications/initialized")
        }
    }

    /// One discovered tool returned by `tools/list`.
    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
    pub struct McpTool {
        pub name: String,
        #[serde(default)]
        pub description: Option<String>,
        /// JSON Schema describing the tool's input. Kept as raw
        /// `Value` because schemas vary per tool and Relix's
        /// admission layer doesn't yet do schema-driven typing.
        #[serde(rename = "inputSchema", default)]
        pub input_schema: Option<Value>,
    }

    /// Result body of `tools/list`.
    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub struct ToolsListResult {
        pub tools: Vec<McpTool>,
    }

    /// Result body of `tools/call`. Per the MCP spec the
    /// `content` array carries the tool's output (text, image,
    /// or resource). `is_error` flags semantic errors the tool
    /// itself reported — distinct from JSON-RPC transport
    /// errors which appear as `JsonRpcError`.
    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub struct ToolsCallResult {
        pub content: Vec<ToolsCallContent>,
        #[serde(rename = "isError", default)]
        pub is_error: bool,
    }

    /// Content element variants. Currently only `text` is
    /// implemented; image / resource land alongside the runtime
    /// that needs them. Unknown content types are accepted via
    /// the catch-all `Other` arm so a forward-compatible server
    /// doesn't break parsing.
    #[derive(Debug, Clone, Deserialize, Serialize)]
    #[serde(tag = "type", rename_all = "lowercase")]
    pub enum ToolsCallContent {
        Text {
            text: String,
        },
        #[serde(other)]
        Other,
    }

    /// Parse one line of newline-delimited JSON into a
    /// `JsonRpcResponse`. Returns a `serde_json::Error` on
    /// invalid JSON or unexpected shape.
    pub fn parse_response_line(line: &str) -> Result<JsonRpcResponse, serde_json::Error> {
        serde_json::from_str(line)
    }

    /// Serialize a request to a single line of newline-delimited
    /// JSON suitable for writing to an MCP server's stdin.
    /// Always terminates with `\n` — the caller never appends.
    pub fn serialize_request(req: &JsonRpcRequest) -> Result<String, serde_json::Error> {
        let mut s = serde_json::to_string(req)?;
        s.push('\n');
        Ok(s)
    }

    /// Same as [`serialize_request`] for notifications.
    pub fn serialize_notification(
        notif: &JsonRpcNotification,
    ) -> Result<String, serde_json::Error> {
        let mut s = serde_json::to_string(notif)?;
        s.push('\n');
        Ok(s)
    }
}

/// Operator-declared MCP server. Lives under `[[tool.mcp.servers]]`
/// in the tool-node config.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpServerConfig {
    /// Stable id the operator uses to refer to this server in
    /// `tool.mcp.invoke`. Must be unique per node config.
    pub id: String,
    /// `"stdio"` (spawn a subprocess and speak the MCP protocol
    /// over stdin/stdout) or `"http"` (POST against an HTTP
    /// endpoint that implements MCP).
    pub transport: String,
    /// For `stdio`: the program to spawn (operator-supplied,
    /// no shell — bare program name like the CW1 terminal
    /// allowlist). For `http`: the base URL.
    pub endpoint: String,
    /// PH-MCP-RUNTIME: explicit `command` override for the stdio
    /// transport. When set the live client uses this instead of
    /// `endpoint`. Lets operators write `command = "npx"` with
    /// `args = ["@modelcontextprotocol/server-filesystem", "/tmp"]`
    /// while keeping `endpoint` as a stable id-like surface.
    /// Ignored for `http` transport.
    #[serde(default)]
    pub command: Option<String>,
    /// PH-MCP-RUNTIME: argv after the program. Only consulted
    /// when `transport = "stdio"`. Empty by default.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional list of tools this server exposes. When set,
    /// `tool.mcp.list_tools` returns this. When None, returns
    /// an empty list (NEVER fabricated). Operators can hand-
    /// curate this until the live discovery path ships.
    #[serde(default)]
    pub declared_tools: Vec<String>,
    /// Short human description for dashboard / logs.
    #[serde(default)]
    pub description: Option<String>,
    /// Environment variables to pass to the spawned process.
    /// Each value can contain `$VAR` references that resolve
    /// against the parent process env at startup; missing
    /// references resolve to empty string (and a startup
    /// `tracing::warn!` line). Ignored for `http` transport.
    /// Example:
    /// ```toml
    /// [[tool.mcp.servers]]
    /// id = "github"
    /// transport = "stdio"
    /// command = "npx"
    /// args = ["-y", "@modelcontextprotocol/server-github"]
    /// [tool.mcp.servers.env]
    /// GITHUB_PERSONAL_ACCESS_TOKEN = "$GITHUB_TOKEN"
    /// ```
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// F13: HTTP transport — optional Authorization header
    /// value the client sends with every request. The
    /// operator writes the FULL header value (e.g.
    /// `"Bearer sk-..."` or `"Basic base64..."`) — Relix
    /// does no envelope wrapping. Empty / unset means no
    /// auth header is sent. Ignored for stdio transport.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// F13: HTTP transport — max number of retry attempts
    /// on transport-level failures (connect / read / 5xx /
    /// 429) per JSON-RPC call. The retry uses exponential
    /// backoff: `100ms * 2^attempt`. Defaults to 3 retries
    /// (4 total attempts). Setting to 0 disables retry.
    /// Ignored for stdio transport.
    #[serde(default = "default_reconnect_max")]
    pub reconnect_max: u32,
}

fn default_reconnect_max() -> u32 {
    3
}

/// Resolve `$VAR` references in `value` against `std::env`.
/// `$` followed by anything not alphanumeric or `_` is left
/// alone. Missing variables resolve to empty string (the
/// caller's tracing line surfaces the gap).
pub fn substitute_env_vars(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        // A variable reference is `$` followed by at least one
        // identifier character; the first must be a letter or
        // underscore (matches POSIX shell variable name rules
        // — `$9.99` is dollar-then-literal, not a var).
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_')
        {
            let mut end = i + 1;
            while end < bytes.len() {
                let b = bytes[end];
                if b.is_ascii_alphanumeric() || b == b'_' {
                    end += 1;
                } else {
                    break;
                }
            }
            let name = &value[i + 1..end];
            let val = std::env::var(name).unwrap_or_default();
            out.push_str(&val);
            i = end;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Resolve every `$VAR` reference in a server's `env` map at
/// once. Returns the resolved key→value map ready to hand to
/// `tokio::process::Command::envs`. Logs a warn line for each
/// `$VAR` that resolved to empty so operators see the gap.
pub fn resolve_env(
    server_id: &str,
    env: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::with_capacity(env.len());
    for (k, v) in env {
        let resolved = substitute_env_vars(v);
        if v.contains('$') && resolved.is_empty() {
            tracing::warn!(
                server = server_id,
                key = k.as_str(),
                template = v.as_str(),
                "mcp: env var resolved to empty — referenced parent env var is missing"
            );
        }
        out.insert(k.clone(), resolved);
    }
    out
}

impl McpServerConfig {
    /// Resolve the program to spawn for `stdio` transport. Returns
    /// `command` when set, otherwise falls back to `endpoint` (the
    /// pre-PH-MCP-RUNTIME shape, kept for backwards compat).
    pub fn stdio_program(&self) -> Option<String> {
        if let Some(c) = &self.command
            && !c.is_empty()
        {
            return Some(c.clone());
        }
        if !self.endpoint.is_empty() {
            return Some(self.endpoint.clone());
        }
        None
    }
}

/// Per-node MCP config. `servers` is empty by default — the
/// `tool.mcp.*` capability family is registered when the
/// `[tool.mcp]` section is present at all, even with no
/// servers, so operators see the surface and can declare
/// servers without restarting.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// Recognised transports.
const KNOWN_TRANSPORTS: &[&str] = &["stdio", "http"];

/// Validate a config — returns the bad entry if any. Used by
/// the manifest registration + the registry construction.
pub fn validate_config(cfg: &McpConfig) -> Result<(), McpError> {
    let mut seen = std::collections::HashSet::new();
    for s in &cfg.servers {
        if s.id.is_empty() {
            return Err(McpError::InvalidConfig {
                reason: "server.id required (non-empty)".into(),
            });
        }
        if !seen.insert(s.id.clone()) {
            return Err(McpError::InvalidConfig {
                reason: format!("duplicate server id: {}", s.id),
            });
        }
        if !KNOWN_TRANSPORTS.contains(&s.transport.as_str()) {
            return Err(McpError::InvalidConfig {
                reason: format!(
                    "server '{}': invalid transport '{}' (allowed: {})",
                    s.id,
                    s.transport,
                    KNOWN_TRANSPORTS.join(", "),
                ),
            });
        }
        if s.transport == "stdio" {
            // PH-MCP-RUNTIME: stdio needs *something* to spawn —
            // either the legacy `endpoint = "<program>"` form or
            // the explicit `command = "..."` field. Both are
            // forbidden from carrying path separators (matches
            // the CW1 terminal allowlist posture).
            let program = s.stdio_program().ok_or_else(|| McpError::InvalidConfig {
                reason: format!(
                    "server '{}': stdio transport requires either `endpoint` or `command`",
                    s.id
                ),
            })?;
            if program.contains('/') || program.contains('\\') {
                return Err(McpError::InvalidConfig {
                    reason: format!(
                        "server '{}': stdio transport requires a bare program name (no path separators); got '{}'",
                        s.id, program
                    ),
                });
            }
        } else if s.transport == "http" {
            if s.endpoint.is_empty() {
                return Err(McpError::InvalidConfig {
                    reason: format!("server '{}': endpoint required for http transport", s.id),
                });
            }
            if !(s.endpoint.starts_with("http://") || s.endpoint.starts_with("https://")) {
                return Err(McpError::InvalidConfig {
                    reason: format!(
                        "server '{}': http transport requires http(s):// URL; got '{}'",
                        s.id, s.endpoint
                    ),
                });
            }
        }
    }
    Ok(())
}

/// MCP registry. Built once at controller startup, shared
/// across handlers. PH-MCP-RUNTIME: the registry now owns
/// per-stdio-server live clients (lazy-spawned). HTTP transport
/// is still RuntimeNotConnected until a separate HTTP client
/// ships. The `clients` map is keyed by `server_id` and only
/// populated for entries whose `transport = "stdio"`.
pub struct McpRegistry {
    servers: Vec<McpServerConfig>,
    /// Lazy-init pool of stdio clients. One entry per stdio
    /// server. Wrapped in a Mutex because clients are added to
    /// the map on first call_tool / list_tools (the clients
    /// themselves serialise their own I/O internally).
    stdio_clients: Mutex<HashMap<String, Arc<McpStdioClient>>>,
    /// F13: pool of HTTP clients. One entry per `http`
    /// server, built up-front in `new` (HTTP clients are
    /// cheap — no I/O until the first request). Reused
    /// across calls so the underlying connection pool can
    /// keep keep-alive sessions warm.
    http_clients: HashMap<String, Arc<McpHttpClient>>,
}

impl McpRegistry {
    pub fn new(cfg: McpConfig) -> Result<Self, McpError> {
        validate_config(&cfg)?;
        let mut http_clients: HashMap<String, Arc<McpHttpClient>> = HashMap::new();
        for s in &cfg.servers {
            if s.transport == "http" {
                let client = super::mcp_http::McpHttpClient::new(
                    s.id.clone(),
                    s.endpoint.clone(),
                    s.auth_header.clone(),
                    s.reconnect_max,
                )
                .map_err(|e| McpError::InvalidConfig {
                    reason: format!("mcp http client for '{}': {e}", s.id),
                })?;
                http_clients.insert(s.id.clone(), Arc::new(client));
            }
        }
        Ok(Self {
            servers: cfg.servers,
            stdio_clients: Mutex::new(HashMap::new()),
            http_clients,
        })
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    pub fn list_servers(&self) -> Vec<McpServerView> {
        self.servers
            .iter()
            .map(|s| McpServerView {
                id: s.id.clone(),
                transport: s.transport.clone(),
                endpoint: s.endpoint.clone(),
                declared_tool_count: s.declared_tools.len(),
                // Honest: stdio servers are "configured" until
                // first call. HTTP servers stay "configured"
                // until the HTTP client ships. The dashboard
                // can grow a richer "connected" projection
                // when it has a reason to.
                status: "configured".to_string(),
                description: s.description.clone(),
            })
            .collect()
    }

    /// Look up the operator-declared config row by id. Internal
    /// helper used by the async handlers.
    fn find_server(&self, server_id: &str) -> Result<&McpServerConfig, McpError> {
        self.servers
            .iter()
            .find(|s| s.id == server_id)
            .ok_or_else(|| McpError::ServerNotFound {
                id: server_id.to_string(),
            })
    }

    /// Get-or-spawn the stdio client for `server_id`. Returns
    /// `None` when the server is not configured as stdio.
    async fn stdio_client_for(
        &self,
        server_id: &str,
    ) -> Result<Option<Arc<McpStdioClient>>, McpError> {
        let s = self.find_server(server_id)?;
        if s.transport != "stdio" {
            return Ok(None);
        }
        let program = s.stdio_program().ok_or_else(|| McpError::InvalidConfig {
            reason: format!(
                "server '{}': stdio transport requires `command` or `endpoint`",
                s.id
            ),
        })?;
        let mut guard = self.stdio_clients.lock().await;
        if let Some(c) = guard.get(server_id) {
            return Ok(Some(c.clone()));
        }
        let client = Arc::new(McpStdioClient::new(s.id.clone(), program, s.args.clone()));
        guard.insert(server_id.to_string(), client.clone());
        Ok(Some(client))
    }

    /// Async list_tools. For stdio AND http servers,
    /// attempts a live `tools/list`; on transport / decode
    /// failure falls back to the operator-declared list (per
    /// the PH-MCP-RUNTIME directive — never fabricated).
    /// Unknown transports return the declared list directly.
    pub async fn list_tools(&self, server_id: &str) -> Result<Vec<String>, McpError> {
        let server = self.find_server(server_id)?;
        let declared = server.declared_tools.clone();
        if server.transport == "stdio"
            && let Some(client) = self.stdio_client_for(server_id).await?
        {
            match client.list_tools().await {
                Ok(res) => return Ok(res.tools.into_iter().map(|t| t.name).collect()),
                Err(e) => {
                    tracing::warn!(
                        server = %server_id,
                        error = %e,
                        "tool.mcp.list_tools stdio call failed; falling back to declared_tools"
                    );
                }
            }
        }
        if server.transport == "http"
            && let Some(client) = self.http_clients.get(server_id).cloned()
        {
            match client.list_tools().await {
                Ok(res) => return Ok(res.tools.into_iter().map(|t| t.name).collect()),
                Err(e) => {
                    tracing::warn!(
                        server = %server_id,
                        error = %e,
                        "tool.mcp.list_tools http call failed; falling back to declared_tools"
                    );
                }
            }
        }
        Ok(declared)
    }

    /// Async invoke. For stdio servers, runs the live
    /// `tools/call` dispatch and returns the encoded
    /// `tools/call` result JSON. For non-stdio transports,
    /// returns RuntimeNotConnected (HTTP runtime is a separate
    /// milestone).
    pub async fn invoke(
        &self,
        server_id: &str,
        tool_name: &str,
        args: &str,
    ) -> Result<Vec<u8>, McpError> {
        // Parse arguments — empty string maps to `{}`. The args
        // form is opaque JSON; the server schema-checks it.
        let args_value: serde_json::Value = if args.trim().is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(args).map_err(|e| McpError::InvalidConfig {
                reason: format!("tool.mcp.invoke: args must be JSON: {e}"),
            })?
        };

        let server = self.find_server(server_id)?;
        match server.transport.as_str() {
            "stdio" => {
                let client = self.stdio_client_for(server_id).await?.ok_or_else(|| {
                    McpError::RuntimeNotConnected {
                        reason: format!("mcp: server '{server_id}' stdio client missing"),
                    }
                })?;
                match client.call_tool(tool_name, args_value).await {
                    Ok(res) => Ok(encode_tools_call_result(&res)),
                    Err(e) => Err(map_stdio_err(&e)),
                }
            }
            "http" => {
                let client = self.http_clients.get(server_id).cloned().ok_or_else(|| {
                    McpError::RuntimeNotConnected {
                        reason: format!(
                            "mcp: http client for server '{server_id}' missing — \
                             registry was constructed without it"
                        ),
                    }
                })?;
                match client.call_tool(tool_name, args_value).await {
                    Ok(res) => Ok(encode_tools_call_result(&res)),
                    Err(e) => Err(map_http_err(&e)),
                }
            }
            other => Err(McpError::RuntimeNotConnected {
                reason: format!("mcp: server '{server_id}' has unknown transport '{other}'"),
            }),
        }
    }

    /// F13: boot-time HTTP discovery. For every `http` server,
    /// runs `initialize` + `tools/list` against the live
    /// endpoint and writes a tracing event with the
    /// discovered tool count. Failures are logged at WARN
    /// level and ignored — a server that's down at boot
    /// doesn't fail the tool node. Live `list_tools` calls
    /// against `tool.mcp.list_tools` already query the
    /// server on every invocation, so this hook is a
    /// warmup + observability layer, not a cache-fill.
    /// Takes `&self` so it can be spawned as a tokio task
    /// against an `Arc<McpRegistry>` without locking.
    pub async fn discover_http_tools(&self) -> usize {
        let mut ok_count = 0usize;
        for s in &self.servers {
            if s.transport != "http" {
                continue;
            }
            let Some(client) = self.http_clients.get(&s.id).cloned() else {
                continue;
            };
            match client.list_tools().await {
                Ok(res) => {
                    tracing::info!(
                        server = %s.id,
                        endpoint = %s.endpoint,
                        tools = res.tools.len(),
                        "mcp http: boot-time discovery succeeded"
                    );
                    ok_count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        server = %s.id,
                        endpoint = %s.endpoint,
                        error = %e,
                        "mcp http: boot-time discovery failed; \
                         keeping operator-declared list"
                    );
                }
            }
        }
        ok_count
    }
}

/// Translate a [`StdioError`] from the live client into the
/// operator-facing [`McpError`] vocabulary. Spawn / EOF /
/// ServerError → RuntimeNotConnected (the runtime *was* meant to
/// be connected; honesty: surface the underlying cause).
/// BadResponse / SerializeRequest → RuntimeNotConnected with
/// a `mcp: bad response` prefix per the directive.
fn map_stdio_err(e: &StdioError) -> McpError {
    match e {
        StdioError::BadResponse(_) | StdioError::LineTooLong { .. } => McpError::BadResponse {
            reason: format!("mcp: bad response: {e}"),
        },
        _ => McpError::RuntimeNotConnected {
            reason: format!("mcp: {e}"),
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerView {
    pub id: String,
    pub transport: String,
    pub endpoint: String,
    pub declared_tool_count: usize,
    pub status: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    #[error("runtime not connected: {reason}")]
    RuntimeNotConnected { reason: String },
    #[error("server not found: {id}")]
    ServerNotFound { id: String },
    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },
    /// PH-MCP-RUNTIME: server responded but the payload was
    /// malformed or violated the protocol. Surfaced as
    /// `RESPONDER_INTERNAL` on the wire (distinct from
    /// `RuntimeNotConnected`, which covers I/O / spawn failures).
    #[error("bad response: {reason}")]
    BadResponse { reason: String },
}

// ─────────────────────────── Capability descriptors ───────────────────────

pub fn descriptor_list_servers() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.mcp.list_servers");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["mcp:registry".into(), "read".into()];
    d.policy_attachment_point = "tool.mcp.list_servers".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description =
        Some("List operator-declared MCP servers + their wire metadata. Pure read.".into());
    d.categories = vec!["mcp".into(), "registry".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

pub fn descriptor_list_tools() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.mcp.list_tools");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["mcp:registry".into(), "read".into()];
    d.policy_attachment_point = "tool.mcp.list_tools".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "List the tool names a given MCP server exposes. For stdio \
         transports runs a live `tools/list` against the spawned \
         subprocess; falls back to the operator-declared list on \
         transport failure or for non-stdio transports."
            .into(),
    );
    d.categories = vec!["mcp".into(), "registry".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

pub fn descriptor_invoke() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.mcp.invoke");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec![
        "mcp:registry".into(),
        "external:process".into(),
        "execute".into(),
    ];
    d.policy_attachment_point = "tool.mcp.invoke".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Invoke a tool on a registered MCP server. For stdio \
         transports the controller spawns the operator-declared \
         subprocess (lazy on first call) and dispatches the call; \
         HTTP transports still return RuntimeNotConnected. Per D-002 \
         the trust-tier decision is still operator-facing."
            .into(),
    );
    d.categories = vec!["mcp".into(), "execute".into()];
    d.risk_level = RiskLevel::High;
    d
}

/// Register every mcp.* capability onto the dispatch bridge.
pub fn register(bridge: &mut DispatchBridge, registry: Arc<McpRegistry>) {
    let r = registry.clone();
    bridge.register(
        "tool.mcp.list_servers",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let r = r.clone();
            async move { handle_list_servers(&r, &ctx) }
        })),
    );
    let r = registry.clone();
    bridge.register(
        "tool.mcp.list_tools",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let r = r.clone();
            async move { handle_list_tools(&r, &ctx).await }
        })),
    );
    let r = registry;
    bridge.register(
        "tool.mcp.invoke",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let r = r.clone();
            async move { handle_invoke(&r, &ctx).await }
        })),
    );
}

// ─────────────────────────── Handlers ───────────────────────────

fn handle_list_servers(reg: &Arc<McpRegistry>, _ctx: &InvocationCtx) -> HandlerOutcome {
    use std::fmt::Write as _;
    let rows = reg.list_servers();
    let mut body = String::new();
    for r in &rows {
        let _ = writeln!(
            body,
            "{}\t{}\t{}\t{}\t{}",
            r.id, r.transport, r.endpoint, r.declared_tool_count, r.status,
        );
    }
    let _ = writeln!(body, "count={}", rows.len());
    HandlerOutcome::Ok(body.into_bytes())
}

async fn handle_list_tools(reg: &Arc<McpRegistry>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim().to_string(),
        Err(e) => return invalid(format!("tool.mcp.list_tools utf8: {e}")),
    };
    if s.is_empty() {
        return invalid("tool.mcp.list_tools: server_id required".into());
    }
    match reg.list_tools(&s).await {
        Ok(tools) => {
            use std::fmt::Write as _;
            let mut body = String::new();
            for t in &tools {
                let _ = writeln!(body, "{t}");
            }
            let _ = writeln!(body, "count={}", tools.len());
            HandlerOutcome::Ok(body.into_bytes())
        }
        Err(e) => to_envelope(&e),
    }
}

async fn handle_invoke(reg: &Arc<McpRegistry>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.to_string(),
        Err(e) => return invalid(format!("tool.mcp.invoke utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    if parts.len() != 3 {
        return invalid(
            "tool.mcp.invoke: arg shape `<server_id>|<tool_name>|<args>` (args may be empty)"
                .into(),
        );
    }
    let server_id = parts[0].trim().to_string();
    let tool_name = parts[1].trim().to_string();
    let args = parts[2].to_string();
    if server_id.is_empty() || tool_name.is_empty() {
        return invalid("tool.mcp.invoke: server_id + tool_name required".into());
    }
    match reg.invoke(&server_id, &tool_name, &args).await {
        Ok(body) => HandlerOutcome::Ok(body),
        Err(e) => to_envelope(&e),
    }
}

fn to_envelope(e: &McpError) -> HandlerOutcome {
    let kind = match e {
        McpError::RuntimeNotConnected { .. } => error_kinds::RESPONDER_INTERNAL,
        McpError::ServerNotFound { .. } => error_kinds::INVALID_ARGS,
        McpError::InvalidConfig { .. } => error_kinds::INVALID_ARGS,
        McpError::BadResponse { .. } => error_kinds::RESPONDER_INTERNAL,
    };
    HandlerOutcome::Err(ErrorEnvelope {
        kind,
        cause: e.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cfg(servers: Vec<McpServerConfig>) -> McpConfig {
        McpConfig { servers }
    }

    fn srv(id: &str, transport: &str, endpoint: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.into(),
            transport: transport.into(),
            endpoint: endpoint.into(),
            command: None,
            args: vec![],
            declared_tools: vec![],
            description: None,
            env: std::collections::HashMap::new(),
            auth_header: None,
            reconnect_max: 0,
        }
    }

    #[test]
    fn substitute_env_vars_replaces_known_and_drops_missing() {
        // SAFE: env is process-global, but the runtime crate
        // forbids unsafe_code and `std::env::set_var` is unsafe
        // in 2024. Test against a real env var that always
        // exists on every Relix-supported platform.
        let path_value = std::env::var("PATH").unwrap_or_else(|_| "fallback".into());
        let resolved = substitute_env_vars("[$PATH]");
        assert_eq!(resolved, format!("[{path_value}]"));
        // Missing var → empty.
        let resolved = substitute_env_vars("token=$RELIX_DEFINITELY_NOT_A_REAL_VAR_XYZZY_999");
        assert_eq!(resolved, "token=");
        // Bare `$` is left alone.
        let resolved = substitute_env_vars("price: $9.99");
        assert_eq!(resolved, "price: $9.99");
    }

    #[test]
    fn resolve_env_returns_empty_for_empty_input() {
        let m = std::collections::HashMap::new();
        let r = resolve_env("test-server", &m);
        assert!(r.is_empty());
    }

    #[test]
    fn resolve_env_substitutes_per_value() {
        let mut m = std::collections::HashMap::new();
        m.insert("LITERAL".into(), "value".into());
        let path_value = std::env::var("PATH").unwrap_or_else(|_| "fallback".into());
        m.insert("INTERP".into(), "$PATH".into());
        let r = resolve_env("test-server", &m);
        assert_eq!(r.get("LITERAL").map(String::as_str), Some("value"));
        assert_eq!(
            r.get("INTERP").map(String::as_str),
            Some(path_value.as_str())
        );
    }

    #[test]
    fn mcp_server_config_parses_with_env_block() {
        let toml = r#"
            id = "github"
            transport = "stdio"
            endpoint = "npx"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-github"]
            [env]
            GITHUB_PERSONAL_ACCESS_TOKEN = "$GITHUB_TOKEN"
        "#;
        let cfg: McpServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.id, "github");
        assert_eq!(cfg.transport, "stdio");
        assert_eq!(cfg.command.as_deref(), Some("npx"));
        assert_eq!(cfg.args.len(), 2);
        assert_eq!(
            cfg.env
                .get("GITHUB_PERSONAL_ACCESS_TOKEN")
                .map(String::as_str),
            Some("$GITHUB_TOKEN")
        );
    }

    #[test]
    fn validate_empty_config_ok() {
        validate_config(&make_cfg(vec![])).unwrap();
    }

    #[test]
    fn validate_rejects_empty_id() {
        let err = validate_config(&make_cfg(vec![srv("", "stdio", "echo")])).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { .. }));
    }

    #[test]
    fn validate_rejects_duplicate_id() {
        let err = validate_config(&make_cfg(vec![
            srv("dup", "stdio", "echo"),
            srv("dup", "http", "http://x"),
        ]))
        .unwrap_err();
        match err {
            McpError::InvalidConfig { reason } => assert!(reason.contains("duplicate")),
            _ => panic!("expected InvalidConfig duplicate"),
        }
    }

    #[test]
    fn validate_rejects_unknown_transport() {
        let err = validate_config(&make_cfg(vec![srv("x", "smoke-signals", "x")])).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { .. }));
    }

    #[test]
    fn validate_rejects_stdio_with_path_separator() {
        let err = validate_config(&make_cfg(vec![srv("x", "stdio", "/usr/bin/mcp")])).unwrap_err();
        match err {
            McpError::InvalidConfig { reason } => assert!(reason.contains("bare program name")),
            _ => panic!("expected InvalidConfig"),
        }
    }

    #[test]
    fn validate_rejects_http_without_scheme() {
        let err = validate_config(&make_cfg(vec![srv("x", "http", "example.com")])).unwrap_err();
        match err {
            McpError::InvalidConfig { reason } => assert!(reason.contains("http(s)://")),
            _ => panic!("expected InvalidConfig"),
        }
    }

    #[test]
    fn registry_lists_servers() {
        let reg = McpRegistry::new(make_cfg(vec![
            srv("a", "stdio", "mcp-srv-a"),
            srv("b", "http", "https://mcp.example.com"),
        ]))
        .unwrap();
        let list = reg.list_servers();
        assert_eq!(list.len(), 2);
        for r in &list {
            assert_eq!(r.status, "configured");
        }
    }

    #[tokio::test]
    async fn registry_list_tools_returns_declared_when_live_call_fails() {
        let mut s = srv("a", "stdio", "relix-mcp-test-no-such-binary-xyzzy");
        s.declared_tools = vec!["search".into(), "fetch".into()];
        let reg = McpRegistry::new(make_cfg(vec![s])).unwrap();
        // Live spawn fails (binary doesn't exist); per the
        // PH-MCP-RUNTIME contract list_tools falls back to the
        // operator-declared list.
        let tools = reg.list_tools("a").await.unwrap();
        assert_eq!(tools, vec!["search".to_string(), "fetch".to_string()]);
    }

    #[tokio::test]
    async fn registry_list_tools_http_returns_declared() {
        let mut s = srv("a", "http", "https://example.com");
        s.declared_tools = vec!["only-declared".into()];
        let reg = McpRegistry::new(make_cfg(vec![s])).unwrap();
        let tools = reg.list_tools("a").await.unwrap();
        assert_eq!(tools, vec!["only-declared".to_string()]);
    }

    #[tokio::test]
    async fn registry_list_tools_unknown_server_errors() {
        let reg = McpRegistry::new(make_cfg(vec![])).unwrap();
        let err = reg.list_tools("nope").await.unwrap_err();
        assert!(matches!(err, McpError::ServerNotFound { .. }));
    }

    #[tokio::test]
    async fn registry_invoke_stdio_spawn_failure_maps_to_runtime_not_connected() {
        let reg = McpRegistry::new(make_cfg(vec![srv(
            "a",
            "stdio",
            "relix-mcp-test-no-such-binary-xyzzy",
        )]))
        .unwrap();
        let err = reg.invoke("a", "search", "{}").await.unwrap_err();
        match err {
            McpError::RuntimeNotConnected { reason } => {
                assert!(reason.starts_with("mcp:"), "reason was: {reason}");
            }
            other => panic!("expected RuntimeNotConnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_invoke_http_returns_runtime_not_connected() {
        let reg =
            McpRegistry::new(make_cfg(vec![srv("a", "http", "https://example.com")])).unwrap();
        let err = reg.invoke("a", "search", "{}").await.unwrap_err();
        assert!(matches!(err, McpError::RuntimeNotConnected { .. }));
    }

    #[tokio::test]
    async fn registry_invoke_unknown_server_first_errors_server_not_found() {
        let reg = McpRegistry::new(make_cfg(vec![])).unwrap();
        let err = reg.invoke("missing", "x", "").await.unwrap_err();
        assert!(matches!(err, McpError::ServerNotFound { .. }));
    }

    #[tokio::test]
    async fn registry_invoke_rejects_non_json_args() {
        let reg = McpRegistry::new(make_cfg(vec![srv("a", "stdio", "echo")])).unwrap();
        let err = reg.invoke("a", "x", "not-json").await.unwrap_err();
        match err {
            McpError::InvalidConfig { reason } => assert!(reason.contains("JSON")),
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn server_config_parses_command_and_args() {
        let toml_src = r#"
            id = "fs"
            transport = "stdio"
            endpoint = "fs"
            command = "npx"
            args = ["@modelcontextprotocol/server-filesystem", "/tmp"]
        "#;
        let s: McpServerConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(s.id, "fs");
        assert_eq!(s.command.as_deref(), Some("npx"));
        assert_eq!(
            s.args,
            vec![
                "@modelcontextprotocol/server-filesystem".to_string(),
                "/tmp".to_string(),
            ]
        );
        assert_eq!(s.stdio_program().as_deref(), Some("npx"));
    }

    #[test]
    fn server_config_stdio_program_falls_back_to_endpoint() {
        let s = srv("a", "stdio", "mcp-srv");
        assert_eq!(s.stdio_program().as_deref(), Some("mcp-srv"));
    }

    #[test]
    fn validate_accepts_stdio_with_command_only() {
        let mut s = McpServerConfig {
            id: "fs".into(),
            transport: "stdio".into(),
            endpoint: "".into(),
            command: Some("npx".into()),
            args: vec!["@modelcontextprotocol/server-filesystem".into()],
            declared_tools: vec![],
            description: None,
            env: std::collections::HashMap::new(),
            auth_header: None,
            reconnect_max: 0,
        };
        // Empty endpoint is fine when command is set.
        validate_config(&make_cfg(vec![s.clone()])).unwrap();
        // Path separators rejected on either field.
        s.command = Some("/usr/bin/npx".into());
        let err = validate_config(&make_cfg(vec![s])).unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig { .. }));
    }

    #[test]
    fn descriptors_carry_mcp_registry_tag() {
        for d in [
            descriptor_list_servers(),
            descriptor_list_tools(),
            descriptor_invoke(),
        ] {
            assert!(
                d.sensitivity_tags.iter().any(|t| t == "mcp:registry"),
                "missing mcp:registry tag on {}",
                d.method_name
            );
        }
    }

    #[test]
    fn invoke_descriptor_includes_execute_tag() {
        let d = descriptor_invoke();
        assert!(d.sensitivity_tags.iter().any(|t| t == "external:process"));
        assert!(d.sensitivity_tags.iter().any(|t| t == "execute"));
    }

    // ── PH-MCP-PROTO: JSON-RPC wire layer ──────────────────────────

    use serde_json::json;

    #[test]
    fn proto_initialize_request_shape_matches_spec() {
        let req = proto::JsonRpcRequest::initialize(1, "relix", "0.1.0");
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, 1);
        assert_eq!(req.method, "initialize");
        let p = req.params.as_ref().unwrap();
        assert_eq!(p["protocolVersion"], "2024-11-05");
        assert_eq!(p["clientInfo"]["name"], "relix");
        assert_eq!(p["clientInfo"]["version"], "0.1.0");
        assert!(p["capabilities"].is_object());
    }

    #[test]
    fn proto_tools_list_request_has_no_params() {
        let req = proto::JsonRpcRequest::tools_list(42);
        assert_eq!(req.method, "tools/list");
        assert!(req.params.is_none());
    }

    #[test]
    fn proto_tools_call_request_carries_name_and_args() {
        let req = proto::JsonRpcRequest::tools_call(7, "search", json!({ "q": "rust" }));
        assert_eq!(req.method, "tools/call");
        let p = req.params.as_ref().unwrap();
        assert_eq!(p["name"], "search");
        assert_eq!(p["arguments"]["q"], "rust");
    }

    #[test]
    fn proto_serialize_request_terminates_with_newline() {
        let req = proto::JsonRpcRequest::tools_list(1);
        let s = proto::serialize_request(&req).unwrap();
        assert!(s.ends_with('\n'), "no trailing newline: {s:?}");
        // The serialized payload (sans newline) is valid JSON.
        let _: serde_json::Value = serde_json::from_str(s.trim_end()).unwrap();
    }

    #[test]
    fn proto_initialized_notification_has_no_id() {
        let n = proto::JsonRpcNotification::initialized();
        let s = serde_json::to_string(&n).unwrap();
        // Notifications must NOT carry an `id` field per JSON-RPC 2.0.
        assert!(!s.contains("\"id\""), "notification has id: {s}");
        assert!(s.contains("notifications/initialized"));
    }

    #[test]
    fn proto_parse_response_line_extracts_result() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"x","version":"0.0.1"}}}"#;
        let resp = proto::parse_response_line(raw).unwrap();
        assert_eq!(resp.id, 1);
        assert!(resp.error.is_none());
        let r = resp.result.as_ref().unwrap();
        assert_eq!(r["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn proto_parse_response_line_extracts_error() {
        let raw =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found"}}"#;
        let resp = proto::parse_response_line(raw).unwrap();
        assert_eq!(resp.id, 2);
        assert!(resp.result.is_none());
        let e = resp.error.as_ref().unwrap();
        assert_eq!(e.code, -32601);
        assert_eq!(e.message, "Method not found");
    }

    #[test]
    fn proto_parse_response_line_rejects_invalid_json() {
        let raw = "not-json";
        assert!(proto::parse_response_line(raw).is_err());
    }

    #[test]
    fn proto_tools_list_result_round_trips_via_value() {
        let raw = json!({
            "tools": [
                { "name": "search", "description": "Search the web",
                  "inputSchema": { "type": "object" } },
                { "name": "fetch" },
            ]
        });
        let r: proto::ToolsListResult = serde_json::from_value(raw).unwrap();
        assert_eq!(r.tools.len(), 2);
        assert_eq!(r.tools[0].name, "search");
        assert!(r.tools[0].description.is_some());
        assert!(r.tools[0].input_schema.is_some());
        assert_eq!(r.tools[1].name, "fetch");
        assert!(r.tools[1].description.is_none());
    }

    #[test]
    fn proto_tools_call_result_parses_text_content() {
        let raw = json!({
            "content": [
                { "type": "text", "text": "hello world" }
            ],
            "isError": false,
        });
        let r: proto::ToolsCallResult = serde_json::from_value(raw).unwrap();
        assert_eq!(r.content.len(), 1);
        assert!(!r.is_error);
        match &r.content[0] {
            proto::ToolsCallContent::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn proto_tools_call_result_handles_unknown_content_type_via_other() {
        let raw = json!({
            "content": [
                { "type": "image", "data": "..." }
            ],
            "isError": false,
        });
        let r: proto::ToolsCallResult = serde_json::from_value(raw).unwrap();
        // Unknown content variants degrade to `Other` so the
        // parser doesn't reject a forward-compatible server.
        assert!(matches!(r.content[0], proto::ToolsCallContent::Other));
    }

    #[test]
    fn proto_tools_call_result_default_is_error_false_when_absent() {
        let raw = json!({
            "content": [
                { "type": "text", "text": "ok" }
            ]
        });
        let r: proto::ToolsCallResult = serde_json::from_value(raw).unwrap();
        assert!(!r.is_error);
    }

    /// PH-RISK-PIN-ALL: pin the risk tier of every MCP
    /// descriptor. Reads of the registry are Safe; invoke
    /// (which spawns / drives an external process when the
    /// stdio runtime is wired) is High. None should be Unknown.
    #[test]
    fn mcp_descriptors_have_explicit_non_unknown_risk() {
        let pinned: &[(&str, CapabilityDescriptor, RiskLevel)] = &[
            (
                "tool.mcp.list_servers",
                descriptor_list_servers(),
                RiskLevel::Safe,
            ),
            (
                "tool.mcp.list_tools",
                descriptor_list_tools(),
                RiskLevel::Safe,
            ),
            ("tool.mcp.invoke", descriptor_invoke(), RiskLevel::High),
        ];
        for (name, d, expected) in pinned {
            assert_ne!(
                d.risk_level,
                RiskLevel::Unknown,
                "{name} defaulted to Unknown risk"
            );
            assert_eq!(
                d.risk_level, *expected,
                "{name} risk tier drifted (expected {expected:?})"
            );
        }
    }
}
