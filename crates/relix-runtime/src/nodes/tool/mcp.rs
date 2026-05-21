//! CW5 — MCP (Model Context Protocol) registry + runtime projection.
//!
//! Hermes ships `mcp_tool` which auto-discovers tools exposed
//! by external MCP servers (stdio or HTTP transport) and
//! projects them into the agent's capability catalog. Relix's
//! CW5 foundation lands the **registry + discovery model**:
//! operators declare MCP servers in their tool-node config,
//! the bridge surfaces connection status + discovered
//! capabilities — but the live execution path returns a
//! typed `RuntimeNotConnected` error until the actual MCP
//! client wiring ships.
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
//! - `[[tool.mcp]]` config entries register servers. Each entry
//!   has `id`, `transport` (`"stdio"` | `"http"`), `command`
//!   or `url`, and an optional `auto_discover` flag.
//! - `tool.mcp.list_servers` returns the operator-declared
//!   server list with `status = "configured"` (never
//!   `"connected"`) until the live client lands.
//! - `tool.mcp.list_tools|<server_id>` returns the
//!   declared / cached tool list (empty when no manual cache
//!   was provided; never fabricated).
//! - `tool.mcp.invoke|<server_id>|<tool_name>|<args>` returns
//!   `RuntimeNotConnected` until the live client wires in.
//!
//! Operators reading the chronicle / audit will never see a
//! fake MCP tool invocation. Per-D-002 the trust-tier
//! decision is still operator-facing and won't be silently
//! resolved.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use relix_core::capability::{CapabilityDescriptor, CapabilityKind, CostClass, Idempotency};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

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
    /// Optional list of tools this server exposes. When set,
    /// `tool.mcp.list_tools` returns this. When None, returns
    /// an empty list (NEVER fabricated). Operators can hand-
    /// curate this until the live discovery path ships.
    #[serde(default)]
    pub declared_tools: Vec<String>,
    /// Short human description for dashboard / logs.
    #[serde(default)]
    pub description: Option<String>,
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
        if s.endpoint.is_empty() {
            return Err(McpError::InvalidConfig {
                reason: format!("server '{}': endpoint required", s.id),
            });
        }
        if s.transport == "stdio" && (s.endpoint.contains('/') || s.endpoint.contains('\\')) {
            return Err(McpError::InvalidConfig {
                reason: format!(
                    "server '{}': stdio transport requires a bare program name (no path separators); got '{}'",
                    s.id, s.endpoint
                ),
            });
        }
        if s.transport == "http"
            && !(s.endpoint.starts_with("http://") || s.endpoint.starts_with("https://"))
        {
            return Err(McpError::InvalidConfig {
                reason: format!(
                    "server '{}': http transport requires http(s):// URL; got '{}'",
                    s.id, s.endpoint
                ),
            });
        }
    }
    Ok(())
}

/// MCP registry. Built once at controller startup, shared
/// across handlers. The registry today is read-only over the
/// config (no live connection state); when the live client
/// lands it'll grow connection-status, last-discovered-at,
/// reconnect counters, etc.
pub struct McpRegistry {
    servers: Vec<McpServerConfig>,
}

impl McpRegistry {
    pub fn new(cfg: McpConfig) -> Result<Self, McpError> {
        validate_config(&cfg)?;
        Ok(Self {
            servers: cfg.servers,
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
                // Honest: "configured" not "connected" — the
                // live client hasn't connected. When the live
                // path lands this projection grows additional
                // status values.
                status: "configured".to_string(),
                description: s.description.clone(),
            })
            .collect()
    }

    pub fn list_tools(&self, server_id: &str) -> Result<Vec<String>, McpError> {
        let s = self
            .servers
            .iter()
            .find(|s| s.id == server_id)
            .ok_or_else(|| McpError::ServerNotFound {
                id: server_id.to_string(),
            })?;
        Ok(s.declared_tools.clone())
    }

    pub fn invoke(
        &self,
        server_id: &str,
        _tool_name: &str,
        _args: &str,
    ) -> Result<String, McpError> {
        // Honesty contract: even if the operator pre-declared
        // the tool, the live execution path hasn't been wired
        // yet. ServerNotFound first (catches operator typos),
        // then RuntimeNotConnected.
        let _ = self
            .servers
            .iter()
            .find(|s| s.id == server_id)
            .ok_or_else(|| McpError::ServerNotFound {
                id: server_id.to_string(),
            })?;
        Err(McpError::RuntimeNotConnected {
            reason: "MCP client runtime is not yet implemented in this Relix build. \
                     The registry + discovery model ships in CW5; live invocation \
                     lands in a follow-up milestone. See docs/mcp-tool.md."
                .to_string(),
        })
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
        "List the tool names a given MCP server exposes. Today reads the \
         operator's `declared_tools` config field; live discovery lands later."
            .into(),
    );
    d.categories = vec!["mcp".into(), "registry".into()];
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
        "Invoke a tool on a registered MCP server. Honesty: returns \
         RuntimeNotConnected today; a follow-up milestone wires the \
         live MCP client. Per D-002 the trust-tier decision is still \
         operator-facing."
            .into(),
    );
    d.categories = vec!["mcp".into(), "execute".into()];
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
            async move { handle_list_tools(&r, &ctx) }
        })),
    );
    let r = registry;
    bridge.register(
        "tool.mcp.invoke",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let r = r.clone();
            async move { handle_invoke(&r, &ctx) }
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

fn handle_list_tools(reg: &Arc<McpRegistry>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("tool.mcp.list_tools utf8: {e}")),
    };
    if s.is_empty() {
        return invalid("tool.mcp.list_tools: server_id required".into());
    }
    match reg.list_tools(s) {
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

fn handle_invoke(reg: &Arc<McpRegistry>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.mcp.invoke utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    if parts.len() != 3 {
        return invalid(
            "tool.mcp.invoke: arg shape `<server_id>|<tool_name>|<args>` (args may be empty)"
                .into(),
        );
    }
    let server_id = parts[0].trim();
    let tool_name = parts[1].trim();
    let args = parts[2];
    if server_id.is_empty() || tool_name.is_empty() {
        return invalid("tool.mcp.invoke: server_id + tool_name required".into());
    }
    match reg.invoke(server_id, tool_name, args) {
        Ok(body) => HandlerOutcome::Ok(body.into_bytes()),
        Err(e) => to_envelope(&e),
    }
}

fn to_envelope(e: &McpError) -> HandlerOutcome {
    let kind = match e {
        McpError::RuntimeNotConnected { .. } => error_kinds::RESPONDER_INTERNAL,
        McpError::ServerNotFound { .. } => error_kinds::INVALID_ARGS,
        McpError::InvalidConfig { .. } => error_kinds::INVALID_ARGS,
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
            declared_tools: vec![],
            description: None,
        }
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

    #[test]
    fn registry_list_tools_returns_declared() {
        let mut s = srv("a", "stdio", "mcp-srv");
        s.declared_tools = vec!["search".into(), "fetch".into()];
        let reg = McpRegistry::new(make_cfg(vec![s])).unwrap();
        let tools = reg.list_tools("a").unwrap();
        assert_eq!(tools, vec!["search".to_string(), "fetch".to_string()]);
    }

    #[test]
    fn registry_list_tools_unknown_server_errors() {
        let reg = McpRegistry::new(make_cfg(vec![])).unwrap();
        let err = reg.list_tools("nope").unwrap_err();
        assert!(matches!(err, McpError::ServerNotFound { .. }));
    }

    #[test]
    fn registry_invoke_returns_runtime_not_connected() {
        let reg = McpRegistry::new(make_cfg(vec![srv("a", "stdio", "mcp-srv")])).unwrap();
        let err = reg.invoke("a", "search", "{}").unwrap_err();
        assert!(matches!(err, McpError::RuntimeNotConnected { .. }));
    }

    #[test]
    fn registry_invoke_unknown_server_first_errors_server_not_found() {
        let reg = McpRegistry::new(make_cfg(vec![])).unwrap();
        let err = reg.invoke("missing", "x", "").unwrap_err();
        assert!(matches!(err, McpError::ServerNotFound { .. }));
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
}
