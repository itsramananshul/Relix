# Architecture

Relix is a mesh of peer processes that talk to each other over libp2p,
plus two small flow languages (`SOL` and `.sflow`) that orchestrate
work across those peers. This document explains how those pieces
compose, what each one is responsible for, and — equally importantly
— what each one is **not** responsible for.

If you want install + boot instructions, start with
[`getting-started.md`](getting-started.md). This document assumes the
mesh is running and you want to know *why* it works the way it does.

## Core invariant: peers, not gateways

Every Relix component is a peer node — a controller process with its
own Ed25519 identity, its own listen address, and its own admission
pipeline. There is no central service.

- The HTTP **bridge** is a peer. It happens to also speak HTTP for the
  benefit of Open WebUI and other OpenAI-compatible clients, but on the
  mesh side it behaves identically to any other peer.
- The built-in controllers — **memory**, **AI**, **tool**,
  **coordinator**, **router**, **plugin_host** — are peers. Each owns
  exactly one concern.
- The optional channels — **telegram**, **discord**, **slack** — are
  peers too. Each polls its platform's REST API and forwards
  conversations into the same SOL chat flow as the HTTP bridge.
- The **operator CLI** (`relix-cli`) is a peer when it makes a call.
  `ping`, `flow-run`, and the per-capability commands spin up an
  ephemeral libp2p client with the operator's identity bundle.

A call from peer A to peer B is a `/relix/rpc/1` request-response
exchange carrying a CBOR-encoded envelope. The transport stack is
TCP + Noise XK + Yamux + CBOR request/response on libp2p 0.54. The
envelope carries the caller's signed identity bundle, the method name,
opaque argument bytes, and a deadline.

## Node types

One binary (`relix-controller`) whose behaviour is selected by
`[controller] node_type` in the per-node TOML. Plus one separate
binary for the HTTP front (`relix-web-bridge`).

| `node_type`    | Default port | Purpose                                                                |
|----------------|--------------|------------------------------------------------------------------------|
| `memory`       | 19711        | SQLite + FTS5 session store, vector embeddings, persistent agent memory |
| `ai`           | 19712        | `ai.chat` / `ai.embed` — provider-agnostic chat + embeddings           |
| `tool`         | 19713        | File system jail, SSRF-guarded web client, allowlisted terminal, headless browser, MCP, PDF, text chunk |
| `coordinator`  | 19714        | Durable Task ledger, delegation, agent-to-agent messaging, scheduler   |
| `telegram`     | 19715        | Telegram bot bridge (opt-in via `RELIX_TELEGRAM=1`)                    |
| `discord`      | 19716        | Discord bot bridge (opt-in via `RELIX_DISCORD=1`)                      |
| `slack`        | 19717        | Slack bot bridge (opt-in via `RELIX_SLACK=1`)                          |
| `plugin_host`  | 19718        | Loads subprocess plugins over `relix-plugin-v1` (opt-in via `RELIX_PLUGINS=1`) |
| `router`       | configurable | Mesh observability + heartbeat aggregator (control plane only — never routes requests) |
| *(bridge)*     | 19791        | HTTP front + OpenAI shim + dashboard — its own binary, not a `node_type` |

Every node — including the bridge — runs the same admission pipeline
on every inbound `/relix/rpc/1` call. See [`security.md`](security.md)
for the pipeline detail.

## Process map

What `scripts/relix-mesh-up.{ps1,sh}` (and therefore `relix boot`)
brings up in the default + opt-in configuration:

```
   OpenAI client / curl / SDK
              │ HTTP
              ▼
   ┌─────────────────────────────────┐    dev-keys/<run>-bridge.aic
   │       relix-web-bridge          │◀── IdentityBundle (chat-users)
   │   127.0.0.1:19791   (HTTP)      │
   │   ephemeral libp2p PeerId       │
   └────────────────┬────────────────┘
                    │ libp2p /relix/rpc/1
   ┌────────────────┼────────────────────────────────────────────┐
   ▼                ▼                ▼            ▼              ▼
┌─────────┐  ┌─────────────┐  ┌─────────┐  ┌─────────────┐  ┌─────────────┐
│ memory  │  │     ai      │  │  tool   │  │ coordinator │  │ plugin_host │
│ :19711  │  │   :19712    │  │ :19713  │  │   :19714    │  │   :19718    │
│ SQLite  │  │  provider   │  │ jail+   │  │ Task ledger │  │ subprocess  │
│ + FTS5  │  │  routing    │  │ SSRF +  │  │ delegation  │  │ plugins via │
│ vectors │  │ ai.chat /   │  │ term +  │  │ msg / cron  │  │ HTTP/JSON   │
│         │  │ ai.embed    │  │ browser │  │             │  │             │
└─────────┘  └─────────────┘  └─────────┘  └─────────────┘  └─────────────┘

   Channels (opt-in; each polls its platform and forwards to the
   chat flow via the memory + ai peers, persists to memory):

   ┌──────────┐  ┌─────────┐  ┌────────┐
   │ telegram │  │ discord │  │ slack  │
   │  :19715  │  │ :19716  │  │ :19717 │
   └──────────┘  └─────────┘  └────────┘
```

Each box is a real OS process with its own PID. The bringup script
launches them in dependency order (memory + ai + tool +
coordinator → opt-in channels + plugin_host → bridge) and records
every PID it spawned so Ctrl-C cleanup is exact. The mesh script
never uses `pkill -f relix-*` — only the PIDs it owns.

### AI → memory back-channel

The arrows above show what callers reach. There's one
**responder-initiated** edge: when the AI controller is configured
with `[ai.memory_peer]`, it dials the memory peer at startup and
the `ai.chat` handler reads up to three things from it per
request:

- `memory.agent_read` — frozen-snapshot agent + user memory
  block prepended to the system prompt.
- `memory.recent_for_session` — automatic conversation history
  for the current session, merged with any caller-supplied
  history.
- `memory.search` — optional RAG retrieval over the vector
  store across **all past sessions**, gated by
  `[ai.memory_peer] rag_enabled = true`. The AI node embeds
  the prompt locally (no libp2p hop) and sends the precomputed
  vector to `memory.search` as a base64 `embedding=` field so
  the memory peer skips its own outbound embed call.

All three are silent-skip on failure — `ai.chat` never blocks
or errors on a degraded memory peer. Wiring detail in
[`memory.md`](memory.md) under *Automatic history injection*
and *RAG (Retrieval-Augmented Generation)*.

## Coordinator (durable Task ledger)

Optional fifth peer. The bridge's chat endpoints persist every request
as a Task on the Coordinator when one is configured:

```
                    bridge
                      │
            task.create / event / update
                      │
                      ▼
              ┌────────────────┐
              │  coordinator   │   SQLite Task ledger
              │  tcp/19714     │   (dev-data/<run>/tasks.db)
              └────────────────┘
```

The bridge does not own Task state — it just **writes through** to
the Coordinator. The Coordinator does not own flow execution — it
just records what other peers attempt. Operators inspect via
`relix-cli task list/get`; details in
[`coordination.md`](coordination.md), [`task-runtime.md`](task-runtime.md),
[`replay-model.md`](replay-model.md).

Fail-soft: a missing or unreachable Coordinator does not block,
crash, or fail any chat request. The bridge logs `WARN coordinator
task.create failed; request persistence skipped` and the response is
returned with `task_id` omitted.

## A request, end to end

What happens when you `POST /v1/chat/completions` against the bridge.

1. **Bridge HTTP handler** (`crates/relix-web-bridge/src/openai.rs`).
   Parses the OpenAI request, sanitises the user content, derives a
   stable `session_id` from the first system+user message, and decides
   which SOL template to render. If the user message contains an
   `http(s)://` URL **and** the tool template is configured, it picks
   [`flows/chat_with_tool.sol`](../flows/chat_with_tool.sol); otherwise
   [`flows/chat_template.sol`](../flows/chat_template.sol). **The
   bridge's only orchestration decision is template selection.** It
   does not plan, retry, or splice tool output. All of that lives in
   SOL.

2. **Bridge flow execution** (`crates/relix-web-bridge/src/flow.rs`).
   Substitutes `{{SESSION}}`, `{{MESSAGE}}`, and (for the tool flow)
   `{{TOOL_URL}}` into the template, writes the rendered SOL to a
   per-request tempfile, and hands a `FlowRunOptions` to the
   `FlowRunner` from `relix-runtime`.

3. **FlowRunner** (`crates/relix-runtime/src/flow_runner.rs`). Compiles
   the SOL source through the ported pipeline (lexer → parser →
   analyzer → codegen) and starts the VM on `tokio::task::spawn_blocking`
   so the synchronous `RemoteCall` opcode can `block_on` the async
   libp2p client. The runtime uses the bridge's pre-existing
   long-lived `MeshClient` instead of spinning up its own transport
   per request.

4. **`RemoteCall` opcode** (`crates/relix-runtime/src/sol/dispatcher.rs`
   + `flow_runner.rs::RealDispatcher::remote_call`). For each
   `remote_call("alias", "method", "args")` in the SOL source:

   - The peer alias is resolved against the pinned `peer_ids` map (or,
     for the `capability:<method>` form, against the bridge's
     discovered capability cache).
   - A `RequestEnvelope` is built, including the caller's
     `IdentityBundle`, the method name, the arg bytes, and a deadline.
   - A `RemoteCallIssued` event is written to the per-flow event log
     (log-before-act).
   - The envelope is sent via `Client::call(peer_id, envelope_bytes)`.
   - The response decode either becomes the SOL string return value,
     or the VM halts with `VM_ERROR_SENTINEL` (every subsequent
     `remote_call` is skipped). The bridge surfaces VM halts as a 502
     with the responder's exact `cause` string.

5. **Responder admission pipeline**
   (`crates/relix-runtime/src/dispatch/mod.rs::DispatchBridge::handle_inbound`).
   The receiving peer's controller runs the same admission pipeline
   on every call, regardless of caller:

   ```
   step 1: decode envelope
   step 3: deadline check
   step 5: validate identity bundle (signed by trusted org root)
   step 7: capability lookup (method registered? else unknown_method)
   step 9: policy evaluation (allowlist DSL, default-deny per method)
   step 10: dispatch to handler
   step 11: write audit record (request_id, caller, method, status,
            decision string, error_kind, started_at -> ended_at)
   ```

   No handler runs unless steps 5, 7, and 9 all pass. Audit is written
   on success **and** failure paths. The audit log is per-node and
   hash-chained.

6. **Handler runs** — `memory.write_turn`, `ai.chat`, or
   `tool.web_fetch`. The handler sees only verified caller identity
   and the raw argument bytes; it cannot bypass policy or audit.

7. **Per-flow event log + audit cross-correlation.** Every
   `RemoteCall` records `RemoteCallIssued` and either
   `RemoteCallCompleted` (with body length + latency) or
   `RemoteCallFailed` (with kind + cause). The per-flow log on the
   *caller* side has the same `request_id` as the responder's audit
   record, so `relix-flow-inspect` can join them.

## The `chat_with_tool` walk-through

Same plumbing, more interesting orchestration. Source:
[`flows/chat_with_tool.sol`](../flows/chat_with_tool.sol).

```
flow start
  ├─ remote_call("memory", "memory.write_turn", "<session>|user|<msg>")
  ├─ remote_call("memory", "memory.recent_for_session", "<session>")
  ├─ remote_call("capability:tool.web_fetch", "tool.web_fetch", "<url>|16384")
  ├─ remote_call("ai", "ai.chat", "<session>|<prompt+fetched body>|<history>")
  ├─ remote_call("memory", "memory.write_turn", "<session>|assistant|<reply>")
  └─ return reply
```

Five real RPCs across three peers. If `tool.web_fetch` returns
`policy_denied` (SSRF reject), the VM halts at step 3; the AI and
final memory writes never happen; the bridge surfaces a 502.

## Components

### `relix-core`

Wire types (`NodeId`, `RequestId`, `TraceId`, `Timestamp`,
`ErrorEnvelope`), the `IdentityBundle` + signing/verification
machinery, the deterministic CBOR codec, the policy engine, the hash-
chained `AuditLog`, the per-flow `EventLog`, and the
`CapabilityDescriptor` type.

No async runtime, no libp2p, no HTTP. This crate is the protocol.

### `relix-runtime`

Everything that runs.

- `transport/` — the libp2p wrapper. `rpc::new(key, port)` returns a
  `Client`, an event receiver, and an `EventLoop` to spawn.
- `dispatch/` — `DispatchBridge` (the admission pipeline above) +
  `Handler` trait.
- `sol/` — the ported SOL VM with the `remote_call` extension.
- `flow_runner.rs` — host-side bridge between the SOL VM and the
  libp2p client; writes the per-flow event log.
- `manifest/` — `NodeManifest`, `ManifestProvider` (built by node-type
  init), `ManifestCache`, and the discovery client `discover_and_pin`
  that hands back both the cache and a long-lived `MeshClient`.
- `nodes/` — node-type implementations: `memory/`, `ai/`, `tool/`,
  `web_bridge/`. Each `register(...)` wires its handlers into the
  dispatch bridge and pushes its descriptors into the manifest
  provider.
- `controller_runtime.rs` — what `relix-controller`'s `main()` calls:
  load identity + trust root + policy, build the dispatch bridge,
  register builtins + node-type handlers, start the transport, dial
  configured peers, and loop on inbound events.

### `relix-controller`

A tiny binary that calls `relix_runtime::controller_runtime::run(config)`.
One binary, many node-types — selected by `[controller] node_type` in
the config TOML.

### `relix-web-bridge`

A separate binary, also a peer. Owns the HTTP surface and the SOL
template render. Holds **no** AI provider keys; never speaks to
external HTTP origins itself (those live on the tool node).

### `relix-cli`

Operator commands.

- `identity init-org --root-key <file> --org <label>` — mint an
  Ed25519 org-root and its companion `.pub` (the trust file).
- `identity mint --root-key <root> --name <subject> --groups <list>
  --out <bundle>` — issue a signed IdentityBundle.
- `identity inspect --bundle <file> --root-key <root>` — print a
  bundle's claims and verification status.
- `ping --peer <multiaddr> --identity <bundle> --client-key <key>` —
  call `node.health` against a peer (full admission pipeline runs).
- `flow-run --flow <path> --identity <bundle> --client-key <key>
  --peers <file>` — compile a SOL file and execute it against a
  configured peer alias map.

### `relix-flow-inspect`

Reads two kinds of log:

- `--audit <path>` — per-node audit log (`dev-data/<node>/audit.log`).
- `--flow <path>` — per-flow event log
  (`dev-data/flow-runner/flows/<flow_id>.log`).

Both formats are CBOR records with a known schema; the tool prints them
as human-readable lines.

## Discovery and capability routing

On startup the bridge runs a one-shot `discover_and_pin` pass against
every entry in its `peers.toml`. For each connected peer it pulls the
peer's `node.manifest` (a built-in capability every controller serves)
and caches the result in a `ManifestCache`. The cache backs two things:

1. `GET /v1/models` — any peer advertising `ai.chat` becomes a
   `relix-<provider>` entry (the provider name lives in the
   capability descriptor's `sensitivity_tags`).
2. The SOL `capability:<method>` peer-alias prefix —
   `remote_call("capability:tool.web_fetch", "tool.web_fetch", arg)`
   asks the dispatcher to consult the cache and route to whichever
   peer advertises the method.

Static aliases (`"memory"`, `"ai"`, etc.) still work; capability:
routing is additive. Manifests are **not signed** in the alpha — that's
a Gate 2 item, documented in [`current-limitations.md`](current-limitations.md).

## Connection reuse

Two layers of pool:

- **Bridge ↔ peers**: one long-lived `MeshClient` per bridge process,
  built once at startup during the discovery pass. Per-request chat
  paths reuse it; the TCP + Noise + Yamux handshake to each peer is
  paid once.
- **Tool peer ↔ origins**: a `PinnedClientPool` of `reqwest::Client`s
  keyed by `(hostname, sorted_validated_addrs)`. Same safe route →
  same Client → reqwest connection pool reuse + TLS state cached.
  Different validated addrs → different Client. The cache key IS the
  validated route, so reuse cannot widen the permitted connect set.
  Details: [`tool-node-security.md`](tool-node-security.md).

## Why the bridge is not an orchestrator

A common temptation is to put the "tool-call detection / re-prompt /
splice" loop in the bridge. We deliberately don't. The reasons:

- **One source of truth.** The SOL flow is the only place that
  describes a multi-step plan. If the same plan lived in two places
  (the flow file and the bridge code), the next person reading the
  flow file would have an incorrect model of what the system does.
- **Bridge is presentation.** Anything that runs in the bridge runs
  outside the responder's admission pipeline. Putting tool selection
  in the bridge would let a bridge bug bypass the tool node's policy
  + SSRF guard + audit. Keeping the bridge dumb means the only way to
  call a capability is through the same admission pipeline as
  everyone else.
- **Frontend independence.** Any OpenAI-compatible client (Open WebUI,
  the openai SDK, curl, a custom UI) gets the same orchestration. We
  do not need to teach every frontend about tool calls.

The trade-off is that the alpha can't ask the LLM "what tool should I
call?" — the flow file picks the tool. Real tool-use integration
(Anthropic-style `tool_use`, OpenAI tools) needs the durable yield
model that lands at Gate 2. The alpha demonstrates the architecture
on a fixed flow; the architecture generalises.

## Streaming and WebSockets

The bridge has two streaming surfaces:

- **`POST /chat/stream`** — Server-Sent Events. Byte-sized
  slices of the assembled reply, used by OpenAI-compatible
  clients (Open WebUI, etc.).
- **`GET /ws/chat`** — WebSocket with JSON frames (`chunk` then
  `done`, or `error`). Requires `Authorization: Bearer <token>`
  on the upgrade. Used by app-level chat UIs.

The `ChatProvider` trait carries a `generate_reply_stream`
method that the mock and OpenAI-compatible providers override —
mock streams word-by-word, OpenAI-compatible parses upstream
`data: <json>\n\n` SSE frames and yields per-token deltas.
End-to-end mesh streaming through `ai.chat` still goes through
the synchronous request/response path; the bridge currently
chunks the materialised reply word-by-word over both endpoints.
Real provider-token-to-client streaming through the mesh lands
post-alpha; the trait surface is already in place to consume it.
See [`websocket.md`](websocket.md).

## Flow languages

Both languages dispatch on file extension via `flow_runner.rs`:

- **`.sol`** — Rust-like imperative DSL. `let x: str = ...;`,
  `if cond { … }`, `while`, `for`, function definitions, `print`,
  `return`. One mesh primitive: `remote_call(peer, method, args)`.
  Use it for chat flows and any logic that benefits from typed
  locals.
- **`.sflow`** — line-oriented step DSL. `step <name>: peer.method
  "arg"`, `set var = "value"`, `${var}` interpolation,
  `if`/`elif`/`else`, `loop N times`, `while` / `until`,
  `try`/`catch`/`rethrow`, plus `sol.log` / `sol.sleep` /
  `sol.assert` / `sol.set_result` built-ins. Use it for operator
  recipes and anything that needs error recovery.

The parser preserves the user-typed dotted method name as
`wire_method`, so `step x: plugin_host.hello.greet "alice"` sends
the full string on the wire and matches the bridge handler
registered under `plugin_host.hello.greet`. Full reference:
[`sol.md`](sol.md).

## Plugin system

`plugin_host` is a controller node type whose handlers are loaded
from subprocesses at boot, not compiled into the binary. The host
reads `plugin.toml` manifests under `--plugin-dir`, spawns each
plugin, reads its `RELIX_PLUGIN_PORT=<n>` line, polls `/health` until
200, then registers every declared capability on its own dispatch
bridge as an `FnHandler` that forwards `POST /invoke` over loopback
HTTP. Each capability is registered under both the bare manifest
name (`hello.greet`) and the prefixed alias (`plugin_host.hello.greet`)
so SOL and `.sflow` callers both reach the same handler. Detail:
[`plugins.md`](plugins.md).

## Next

- [`sol.md`](sol.md) — full reference for SOL and `.sflow`.
- [`security.md`](security.md) — identity, policy, audit, the
  admission pipeline.
- [`configuration.md`](configuration.md) — every config knob.
- [`coordination.md`](coordination.md) — multi-agent tasks,
  delegation, messaging, approvals.
- [`channels/index.md`](channels/index.md) — telegram, discord, slack.
- [`plugins.md`](plugins.md) — plugin protocol, SDK, lifecycle.
- [`operator-guide.md`](operator-guide.md) — running, logging,
  troubleshooting.
