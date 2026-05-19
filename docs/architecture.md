# Architecture

Relix is a small set of peer processes that talk to each other over
libp2p, plus a SOL VM that runs hand-written flow files. This document
explains how those pieces compose, what each one is responsible for,
and — equally importantly — what each one is **not** responsible for.

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
- The **memory**, **AI**, and **tool** nodes are peers. Each owns
  exactly one concern: SQLite + FTS5; provider routing; SSRF-guarded
  HTTP fetch.
- The **operator CLI** (`relix-cli`) is a peer when it makes a call.
  `ping` and `flow-run` spin up an ephemeral libp2p client with the
  operator's identity bundle.

A call from peer A to peer B is a `/relix/rpc/1` request-response
exchange carrying a CBOR-encoded RELIX-1 envelope. The transport is
TCP + Noise XK + Yamux + CBOR request/response (libp2p 0.54). The
envelope carries the caller's signed identity bundle, the method name,
opaque argument bytes, and a deadline.

## Process map

A typical local mesh, exactly what
[`scripts/relix-mesh-up.ps1`](../scripts/relix-mesh-up.ps1) brings up:

```
┌────────────────────────────┐
│         Open WebUI         │
│  (or curl / SDK / shim)    │
└─────────────┬──────────────┘
              │ HTTP
              ▼
┌────────────────────────────┐         dev-keys/<run>-bridge.aic
│      relix-web-bridge      │◀──────── IdentityBundle (group: chat-users)
│  127.0.0.1:19791  (HTTP)   │
│  ephemeral libp2p PeerId   │
└─────────────┬──────────────┘
              │ libp2p /relix/rpc/1
   ┌──────────┴──────────┐
   │                     │
   ▼                     ▼
┌────────────────┐  ┌────────────────┐  ┌────────────────┐
│ relix-         │  │ relix-         │  │ relix-         │
│ controller     │  │ controller     │  │ controller     │
│ node_type =    │  │ node_type =    │  │ node_type =    │
│ "memory"       │  │ "ai"           │  │ "tool"         │
│ tcp/19711      │  │ tcp/19712      │  │ tcp/19713      │
│ SQLite+FTS5    │  │ provider key   │  │ reqwest + SSRF │
└────────────────┘  └────────────────┘  └────────────────┘
        ▲                  ▲                    ▲
        │                  │                    │
        └────── memory.*  ai.chat  tool.web_fetch (+ node.health, node.manifest)
```

Each box is a real OS process with its own PID. The bringup script
launches them in order (memory + AI + tool first, then the bridge once
the controllers are listening) and records every PID it spawned so
Ctrl-C cleanup is exact.

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

## Next

- [`flows-and-sol.md`](flows-and-sol.md) — what SOL is, how to write a
  new flow.
- [`security.md`](security.md) — identity, policy, audit, what the
  alpha guarantees and what it doesn't.
- [`operator-guide.md`](operator-guide.md) — running, logging,
  troubleshooting.
