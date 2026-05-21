# MCP tool (CW5)

`tool.mcp.*` ships the **registry + discovery surface** for the
Model Context Protocol. As of this milestone there is **no
live MCP client**: `tool.mcp.invoke` returns a typed
`RuntimeNotConnected` error. The registry, capability
descriptors, validation, dispatch path, and dashboard surface
are real. A future milestone wires the stdio + HTTP MCP
clients behind the same registry.

## Honesty contract

> If actual MCP execution requires a later runtime decision,
> build the registry/discovery model first and label execution
> as not connected yet. No fake MCP execution.

The operator can:

- Declare MCP servers in `[tool.mcp]` config (id, transport,
  endpoint, optional `declared_tools`).
- Call `tool.mcp.list_servers` — each row's `status` reads
  `configured` (never `connected`).
- Call `tool.mcp.list_tools|<server_id>` — returns the
  operator-supplied `declared_tools` (never fabricated).

The operator CANNOT today:

- Call `tool.mcp.invoke|<server_id>|<tool_name>|<args>` — returns
  `RuntimeNotConnected` with the reason "MCP client runtime is
  not yet implemented in this Relix build."

## Config

```toml
[tool.mcp]
# Each entry registers one MCP server. The bridge surfaces it
# in `tool.mcp.list_servers`. `id` must be unique per node.

[[tool.mcp.servers]]
id          = "fs-helper"
transport   = "stdio"
endpoint    = "mcp-fs-server"     # bare program name; no paths
description = "Local filesystem MCP server (operator-supplied)"
declared_tools = ["search", "read", "write"]

[[tool.mcp.servers]]
id          = "remote-search"
transport   = "http"
endpoint    = "https://mcp.example.com"
declared_tools = []
```

Validation enforced at startup:

- `id` non-empty + unique.
- `transport` ∈ `{"stdio", "http"}`.
- `stdio` endpoints must be bare program names (no path separators).
- `http` endpoints must start with `http://` or `https://`.

When `[tool.mcp]` is absent the capability family is NOT
registered.

## Why ship the registry before the client?

1. **Operator visibility**: dashboard + CLI show declared
   servers + their tools today. Operators can review the
   wiring before the client lands.
2. **Honesty over fake-success**: a stub client that returned
   `{"result":"ok"}` would lie about the integration state.
   `RuntimeNotConnected` makes the gap impossible to miss.
3. **Stable contract**: the wire format and trust model are
   pinned. The live client slots into `McpRegistry::invoke`
   without touching the dispatch path.

## D-002 — trust tier decision (open)

Per `docs/internal/decisions-pending.md` D-002, MCP servers
are operator-curated only today. The runtime does NOT
automatically enable any server. Future questions for the
operator:

- Should the registry support per-server trust tiers (`trusted`
  vs `community`) like Hermes's ClawHub?
- Should the bridge auto-validate server health (TCP probe / MCP
  initialize handshake) before letting `tool.mcp.invoke` reach it?
- Should `chat-users` ever invoke MCP tools, or stay `operators` only?

These land in the live-client milestone, not this scaffold.

## Future milestones

- **CW5-A**: stdio MCP client. Spawn the configured program,
  speak MCP over stdin/stdout, project discovered tools.
- **CW5-B**: HTTP MCP client. POST against the configured URL.
- **CW5-C**: live capability discovery — replace the
  `declared_tools` static cache with what the server actually
  advertises at handshake time.
- **CW5-D**: dashboard MCP explorer — list servers + per-server
  tools + connection health.
- **CW5-E**: telemetry counters per server (invocations,
  failures, latency).
- **CW5-F**: per-server quarantine + auto-cooldown (mirror the
  AI-provider rate-limit ladder PH-WAVE2I/J).
