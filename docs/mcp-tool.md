# MCP tool (CW5)

`tool.mcp.*` ships the **registry + discovery surface** for the
Model Context Protocol plus a **live stdio runtime**
(PH-MCP-RUNTIME, D-009 closed). HTTP transport still returns
`RuntimeNotConnected` until the HTTP client lands.

Relix targets **MCP protocol version `2024-11-05`** (sent in the
`initialize` request's `protocolVersion` field).

## Honesty contract

> If actual MCP execution requires a later runtime decision,
> build the registry/discovery model first and label execution
> as not connected yet. No fake MCP execution.

The operator can:

- Declare MCP servers in `[tool.mcp]` config (id, transport,
  endpoint, optional `command`/`args`, optional
  `declared_tools`).
- Call `tool.mcp.list_servers` — each row's `status` reads
  `configured` (the dashboard projection grows richer states
  when live health checks ship).
- Call `tool.mcp.list_tools|<server_id>` — for `stdio`
  transports the bridge runs a live `tools/list`; on transport
  failure it falls back to the operator-declared list (never
  fabricated). For other transports the declared list is
  returned.
- Call `tool.mcp.invoke|<server_id>|<tool_name>|<args>` — for
  `stdio` transports the bridge spawns the configured
  subprocess (lazy: on first call), runs the MCP `initialize`
  handshake, dispatches `tools/call`, and returns the encoded
  result content as JSON bytes. Spawn / I/O failures surface as
  `RuntimeNotConnected` with the underlying cause prefixed
  `mcp:`. Malformed responses surface as `RESPONDER_INTERNAL`
  with `mcp: bad response: ...`. No fake success ever.

The operator CANNOT today:

- Call `tool.mcp.invoke` against an `http` transport server —
  returns `RuntimeNotConnected` until the HTTP client ships.

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

# PH-MCP-RUNTIME: explicit `command` + `args` lets you run
# servers shipped via package managers (npx, uvx, etc.).
# `endpoint` stays as the stable id-surface; `command` wins
# when set.
[[tool.mcp.servers]]
id          = "fs-everything"
transport   = "stdio"
endpoint    = "everything"
command     = "npx"
args        = ["-y", "@modelcontextprotocol/server-everything"]

[[tool.mcp.servers]]
id          = "remote-search"
transport   = "http"
endpoint    = "https://mcp.example.com"
declared_tools = []
```

Validation enforced at startup:

- `id` non-empty + unique.
- `transport` ∈ `{"stdio", "http"}`.
- `stdio` requires `command` OR `endpoint`; whichever resolves
  must be a bare program name (no path separators).
- `http` endpoints must start with `http://` or `https://`.

When `[tool.mcp]` is absent the capability family is NOT
registered.

**Boot-time HTTP discovery:** when the tool node starts and `[tool.mcp]`
is present, HTTP-transport servers are probed via a non-blocking
`tokio::spawn` call (`McpRegistry::discover_http_tools`). This does
**not** block tool node startup — a probe failure is logged but does not
prevent the node from accepting other requests.

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

- **CW5-A**: stdio MCP client — **shipped** (PH-MCP-RUNTIME, D-009
  closed). `tool.mcp.invoke` against `stdio` transport servers now
  spawns the subprocess, runs the MCP `initialize` handshake, and
  dispatches `tools/call` live.
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
