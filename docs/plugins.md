# Plugin system

Third-party code can extend Relix without modifying the core
codebase. A plugin is a separate subprocess that exposes one or
more capabilities to the mesh via a small HTTP/JSON protocol
(`relix-plugin-v1`). The `plugin_host` node type loads plugins
at boot, registers their capabilities on its dispatch bridge,
and acts as a normal mesh peer for the rest of the system.

```
   ┌──────────────────────────────────────────────────────────┐
   │ plugin_host controller (node_type = "plugin_host")       │
   │                                                          │
   │   ┌──────────────┐     ┌──────────────┐                  │
   │   │ DispatchBridge│◄──►│ plugin.list  │   (management)   │
   │   │              │     │ plugin.status│                  │
   │   │              │     │ plugin.reload│                  │
   │   │              │     │ plugin.disable│                 │
   │   │              │     └──────────────┘                  │
   │   │              │                                       │
   │   │              │      HTTP /invoke                     │
   │   │              ├─────────────────────► [hello-plugin]  │
   │   │              ├─────────────────────► [web-lookup]    │
   │   │              ├─────────────────────► …               │
   │   └──────────────┘                                       │
   │                                                          │
   └──────────────────────────────────────────────────────────┘
```

## When you'd reach for this

- **You need a capability the built-in node types don't have.**
  e.g. wrap a third-party REST API, a database query surface, a
  local LLM tool runtime, an SSO callback handler.
- **You want to ship something written in a non-Rust language.**
  Python, Go, or anything else that can speak HTTP. The Rust
  SDK is provided as a convenience; the protocol is the contract.
- **You want capability-level isolation.** Plugins live in their
  own subprocesses. A panicking plugin can't take the rest of
  the node down.

## Protocol: `relix-plugin-v1`

Plugins run as subprocesses spawned by a `plugin_host`. The
host pipes the plugin's stdout so it can find the announced
port, then talks to the plugin over HTTP on localhost.

### Startup contract

1. The plugin binds an HTTP server on `127.0.0.1:<port>` where
   `<port>` is **kernel-chosen** (bind to `127.0.0.1:0`).
2. On its **first line of stdout**, the plugin writes:
   ```
   RELIX_PLUGIN_PORT=<port>
   ```
3. The host reads that line, then starts polling `/health` until
   it returns `200`. Default deadline: 10s for the port line,
   30s for `/health` to become 200.
4. Once healthy, the host registers each capability declared in
   the manifest on its dispatch bridge as a `FnHandler` that
   routes incoming calls to `POST /invoke` on the plugin.

### Endpoints

| Method | Path | Body | Purpose |
|---|---|---|---|
| GET  | `/health` | — | `{ "ok": true }` once the server is up |
| GET  | `/ready`  | — | `{ "ok": true }` once warm |
| POST | `/invoke` | `InvokeRequest` JSON | Capability dispatch |

### Invoke request

```json
{
  "method":            "my_plugin.do_thing",
  "args":              "pipe|delimited|string",
  "trace_id":          "<hex16>",
  "request_id":        "<hex16>",
  "caller_subject_id": "<hex32>",
  "deadline_unix":     1700000000
}
```

### Invoke response

Success:
```json
{ "ok": true, "body": "result string" }
```

Failure:
```json
{
  "ok":          false,
  "error_kind":  11,
  "error_cause": "human-readable cause"
}
```

`error_kind` mirrors `relix_core::types::error_kinds`. The
common ones a plugin returns:

| Kind | Constant | Meaning |
|---|---|---|
| 4  | `UNKNOWN_METHOD` | Host's manifest is out of sync; plugin has no handler for `method` |
| 5  | `INVALID_ARGS` | Caller passed malformed / missing args |
| 11 | `RESPONDER_INTERNAL` | Plugin's own error — panic-recovered, bad downstream response, etc. |
| 12 | `RESPONDER_OVERLOADED` | Plugin's upstream is rate-limited; caller may retry |

## `plugin.toml` reference

```toml
[plugin]
name        = "my-plugin"      # lowercase + hyphens + digits, 3..=64 chars
version     = "0.1.0"
description = "What this plugin does"
author      = "Author Name"     # optional
homepage    = ""                # optional
license     = "Apache-2.0"      # optional

# At least one provides entry is required.
[[plugin.capabilities.provides]]
method            = "my_plugin.do_thing"     # dotted [a-z][a-z0-9_]*
description       = "Does a thing"
categories        = ["tool", "external"]     # optional
sensitivity_tags  = ["external:api"]         # optional
risk_level        = "low"                    # low | medium | high

[plugin.runtime]
kind                 = "subprocess"          # only "subprocess" today
binary               = "./my-plugin-binary"  # see Binary resolution below
args                 = ["--serve"]           # optional
protocol             = "relix-plugin-v1"     # only "relix-plugin-v1"
invoke_timeout_secs  = 30                    # 1..=300; default 30
```

### Binary resolution

- **Bare name** (`binary = "python"`) — passed verbatim to
  `Command::new`, which uses the system PATH. Use this when the
  plugin runs under an interpreter installed system-wide.
- **Absolute path** (`binary = "/opt/my-plugin/bin/serve"`) —
  used as-is.
- **Relative path** (`binary = "./my-plugin-binary"`) — resolved
  against the manifest directory.

## Writing a plugin in Rust (the SDK)

Add the SDK as a dependency:

```toml
[dependencies]
relix-plugin-sdk = "0.1"   # or = { path = "../relix-plugin-sdk" }
tokio            = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Tiny example:

```rust
use relix_plugin_sdk::{InvokeRequest, PluginError, PluginServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut server = PluginServer::new();
    server.register("my_plugin.greet", |req: InvokeRequest| async move {
        if req.args.is_empty() {
            return Err(PluginError::invalid_args("name required"));
        }
        Ok(format!("hello, {}", req.args))
    });
    server.serve().await?;
    Ok(())
}
```

See `examples/plugins/web-lookup/src/main.rs` for a complete
plugin that wraps a real HTTP API.

## Writing a plugin in Python

Stdlib only — no third-party deps. See
`examples/plugins/hello-plugin/hello.py` for the full source.
Key points: bind `("127.0.0.1", 0)`, print
`RELIX_PLUGIN_PORT=<n>` as the first stdout line, serve
`GET /health` + `POST /invoke`.

## Host config: `[plugin_host]`

```toml
[controller]
node_type   = "plugin_host"
listen_port = 19718

[plugin_host]
plugin_dir       = "./plugins"                    # directory scanned at boot
max_plugins      = 20                             # safety cap
registry_db_path = "dev-data/plugin-registry.db"  # SQLite registry
```

The host walks `plugin_dir` at depth 1, accepting either:

- `plugin_dir/plugin.toml` (single plugin), or
- `plugin_dir/<name>/plugin.toml` (one subdir per plugin).

## Calling a plugin from a flow

Both SOL and `.sflow` work. The plugin_host bridge registers
each capability under two names — the bare manifest name and a
`plugin_host.<method>` alias — so callers can use whichever
form is natural for the language.

SOL — call by the bare manifest name:

```rust
// flows/hello.sol
function start() -> str {
    let reply: str = remote_call("plugin_host", "hello.greet", "alice");
    return reply;
}
```

`.sflow` — the parser preserves the full dotted target as the
wire method, so the natural `plugin_host.<method>` form admits
correctly against the prefixed alias:

```
step reply: plugin_host.hello.greet "alice"
return step.reply.result
```

The same applies to plugin management capabilities — they're
callable from sflow as `plugin_host.plugin.list` /
`plugin_host.plugin.status` / etc.

## Management capabilities + HTTP / CLI

| Capability | HTTP | CLI |
|---|---|---|
| `plugin.list` | `GET /v1/plugins` | `relix-cli ops plugin list` |
| `plugin.status` | `GET /v1/plugins/:id` | `relix-cli ops plugin status --plugin-id <id>` |
| `plugin.reload` | `POST /v1/plugins/:id/reload` | `relix-cli ops plugin reload --plugin-id <id>` |
| `plugin.disable` | `POST /v1/plugins/:id/disable` | `relix-cli ops plugin disable --plugin-id <id>` |

The dashboard `#/plugins` page shows the same data with a
clickable list + a detail card with Reload / Disable buttons.

## Lifecycle states

| State | What it means | How to reach |
|---|---|---|
| `registered` | Manifest parsed + stored. Subprocess not running. | First scan; failed reload |
| `active`     | Subprocess up; `/health` returned 200. Capabilities live. | Successful spawn |
| `error`      | Spawn or health probe failed. `error_message` describes. | Subprocess failed to start, exited, or `/health` never returned 200 within 30s |
| `disabled`   | Operator explicitly stopped it. | `plugin.disable` |

## Security posture

- **Subprocess isolation.** Plugins run in their own OS process.
  A panicking plugin can't take the plugin_host down. Killing
  the plugin_host kills the children (tokio
  `Command::kill_on_drop(true)`).
- **Capability gating through the policy engine.** Every method
  a plugin registers passes through the same `PolicyEngine`
  admission as built-in capabilities. Operators write rules
  for plugin methods in the same TOML they already use:
  ```toml
  [[rules]]
  name         = "my_plugin_do_thing"
  method       = "my_plugin.do_thing"
  allow_groups = ["chat-users"]
  ```
- **No automatic credential sharing.** A plugin process gets
  its own environment. The host does not inject any of its own
  identity, mesh peer credentials, or provider API keys. If a
  plugin needs an API key, the operator sets it in the plugin's
  own env at startup.
- **No mesh trust escalation.** A plugin returning `ok: true`
  doesn't bypass the dispatch bridge's audit log, sensitivity
  tags, or admission steps. The plugin_host treats plugin
  responses the same way it would treat any other handler's
  outcome.

## Deployment notes

- **Plugins must respect `deadline_unix`.** The host sets it
  from `now + invoke_timeout_secs`; plugins should short-circuit
  past it. Today the SDK doesn't enforce — well-behaved plugins
  check `deadline_unix < now` and bail early.
- **Long-running work belongs in the plugin.** The host's
  per-call deadline (default 30s) is a hard ceiling. For a
  multi-minute background task, the plugin should kick off the
  work asynchronously and expose a separate capability to poll
  for results.
- **Plugins can crash.** The host detects this on the next
  invoke (Transport error) and surfaces a 502 to callers. The
  plugin_host does not automatically restart failed plugins —
  use `plugin.reload` (`/v1/plugins/:id/reload`) or restart the
  plugin_host node.
- **The registry survives restarts.** `plugin-registry.db`
  carries `(plugin_id, status, error_message, last_seen_at)`
  across reboots so the dashboard shows persistent history.
