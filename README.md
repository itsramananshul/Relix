# Relix

Relix is a peer-to-peer distributed runtime for AI orchestration. Every channel, AI provider, memory store, tool, and enterprise system is a peer **node** on a P2P mesh. Routing is expressed in **SOL** programs that compile to bytecode and execute inside each peer's controller daemon. There is no central gateway, no central credential store, no central registry.

This repository is the broad alpha: every major platform piece is present and honest, with depth and edge-case hardening simplified for the one-week timebox. The architecture is production-seed; the simplifications are documented in [`specs/alpha-simplifications.md`](specs/alpha-simplifications.md) and are removable without rewriting the platform.

## What's In the Alpha

- A Rust **controller daemon** that hosts capabilities, signs events, evaluates policy, and audits every responder action.
- **Real peer-to-peer**: separate OS processes talking to each other over a CBOR-framed transport. (Alpha uses TCP+CBOR; libp2p slots into the same `Transport` trait at Gate 2.)
- **Signed identities**: Ed25519 `IdentityBundle` (alpha equivalent of AIC), one-level delegation chain.
- **Group-based policy** evaluated on every responder before any handler runs. Allowlist DSL now; Cedar at Gate 2.
- **Hash-chained event log** per flow + audit indexing.
- **SOL** as the routing source of truth. Synchronous `remote_call` opcode in the alpha; durable yield model at Gate 2.
- **Memory node** (`memory.search` / `memory.write_turn` / `memory.recent_for_session`) backed by SQLite + FTS5, ported from Hermes's session-storage approach.
- **AI node** (`ai.chat`) wrapping Anthropic; Anthropic key lives only in the AI node's local config.
- **Tool node** (`tool.web_fetch`) — distributed external-action peer that fetches a single URL with an SSRF guard (scheme + literal-IP + DNS-resolved-address + redirect cap + body cap, all fail-closed). HTTPS by default; `http` opt-in. See [`docs/tool-node-security.md`](docs/tool-node-security.md).
- **Web bridge** exposing local SSE to **Relix Web** (an Open WebUI fork in `relix-web/`) — the web peer never holds AI provider keys.
- A canonical agent flow (`flows/chat.sol`) and a tool-aware variant (`flows/chat_with_tool.sol`).

## What's NOT In the Alpha (And Won't Be Sneaked In)

- No marketplace.
- No central gateway. No "Relix Cloud."
- No credentials in the web backend. The web peer is presentation only.
- No routing decisions outside SOL. No `if method == "ai.chat"` in Rust or Python.
- No HSM, no IA hierarchy, no federation between organizations, no SolFlow live mode, no mobile peers, no voice, no image generation, no general MCP, no auto-discovery of tools.

These are deferred per the alpha plan, not negotiated away.

## Layout

| Path | Purpose |
|---|---|
| `specs/` | Source of truth (`RELIX-1`..`RELIX-8` substrate specs + `alpha-simplifications.md`). |
| `crates/` | Rust workspace — the controller, transport, runtime, and node crates. |
| `relix-web/` | Open WebUI fork; Relix Web. Python + SvelteKit. Not a Cargo member. |
| `flows/` | Hand-written SOL flows for the alpha. |
| `configs/` | Example node config files. |
| `conformance/` | Wire-format test vectors. |
| `tools/` | `relix-cli`, `relix-flow-inspect`. |
| `ops/runbooks/` | Operator procedures. |
| `docs/` | Architecture, code-reuse map, security-critical deps, alpha plan. |

## Quickstart (M5 — what works today)

Two real OS processes (controller + CLI) talking over libp2p with signed identity, per-call policy, and audited responses:

```sh
# One command runs the whole demo (POSIX / git-bash).
./scripts/alpha-bringup-m5.sh
# Or on Windows PowerShell:
./scripts/alpha-bringup-m5.ps1
```

The script:
1. Mints an org root, plus an `alice` AIC (`chat-users`) and a `bob` AIC (`guest`).
2. Starts a controller on `tcp/19501` with `node.health` policy that admits `chat-users`.
3. Pings as alice → expects `OK` + structured `node.health` payload.
4. Pings as bob → expects `policy_denied`.
5. Prints the responder's audit log (both records joinable by `request_id`).

Manual command shape:

```sh
cargo run -p relix-cli -- ping \
    --peer /ip4/127.0.0.1/tcp/9001 \
    --identity dev-keys/alice.aic \
    --method node.health \
    --client-key dev-keys/org-root.key
```

### M6 — SOL flow `remote_call` (also working today)

```sh
./scripts/alpha-bringup-m6.sh           # single peer
./scripts/alpha-bringup-m6-chained.sh   # two peers, two sequential remote_calls
```

`alpha-bringup-m6.sh` compiles `flows/ping.sol` and runs it against one controller; the denied path (Bob with `guest` group) exits 2.

`alpha-bringup-m6-chained.sh` runs `flows/chained_health.sol` against two real controller processes (memory + ai) in sequence. Alice's happy-path flow log has 6 events (`FlowStarted` → `RemoteCallIssued(memory)` → `RemoteCallCompleted` → `RemoteCallIssued(ai)` → `RemoteCallCompleted` → `FlowCompleted`); Bob's denied flow short-circuits at the first call with 4 events.

### M7 — memory node + conversational orchestration (working today)

```sh
./scripts/alpha-bringup-m7-memory.sh    # memory CRUD over the M5 admission pipeline
./scripts/alpha-bringup-m7-chat.sh      # first end-to-end agent flow: memory + AI
```

The memory demo writes two turns to a real SQLite + FTS5 backend and reads history back. The chat demo orchestrates **two real controller processes** (memory + AI) from a 4-call SOL flow in a conversational state machine: persist user turn → read recent history → AI call → persist assistant turn. Alice's flow log has 10 events in exact order; bob (`guest`) is denied at the first call. A fifth verification call confirms both turns landed in the SQLite store.

The AI node is **provider-agnostic**: pick one of `mock` (default; deterministic; no secrets), `openai`, `openrouter`, `xai`, `local` (Ollama / vLLM / llama.cpp), `anthropic`, or `gemini` (placeholder). All provider keys are loaded from named env vars (`api_key_env = "VAR_NAME"`) and live only on the AI node. The SOL flow is identical across providers. See [`docs/provider-configuration.md`](docs/provider-configuration.md).

### M8 — local web bridge (working today)

```sh
./scripts/alpha-bringup-m8-web-bridge.sh
```

A small axum service on `127.0.0.1:9100` exposes `POST /chat` and `GET /health`. The bridge is a normal Relix peer with its own identity bundle — it holds **no** AI provider key, never bypasses identity/policy, and never orchestrates in Rust. Each request renders `flows/chat_template.sol` with the JSON-supplied `session_id`/`message` and runs it through the existing `FlowRunner`, returning `{reply, flow_id, trace_id, flow_log}` JSON. Input validation rejects `"`, `|`, and newlines (the only characters that could break out of a SOL string literal under SIMP-018).

### M8/S2 — streaming + Open WebUI (working today)

```sh
# Self-running smoke demo: boots the mesh, runs canned curls, tears down.
./scripts/alpha-bringup-m8-openwebui.sh

# Operator boot driver: boots the mesh and PARKS until Ctrl-C. Use this when
# you want to talk to the mesh from your own curl, Open WebUI, or any
# OpenAI-compatible client.
./scripts/relix-mesh-up.sh                       # mock provider, no keys
./scripts/relix-mesh-up.sh --provider openrouter # needs $OPENROUTER_API_KEY
./scripts/relix-mesh-up.sh --provider local --base-url http://localhost:11434/v1
```

Two new endpoint shapes ship on the same bridge so any OpenAI-compatible client can talk to Relix unchanged:

- `POST /chat/stream` — Relix-native SSE (`event: chunk` × N, then `event: done` with a provenance JSON payload).
- `POST /v1/chat/completions` (+ `GET /v1/models`) — OpenAI Chat Completions shape, supporting both non-streaming JSON and `stream:true` SSE. A stable `session_id` is derived from the first system + user message so the same conversation lands in the same Relix-memory bucket as it grows.

Streaming is **bridge-level chunking** of an already-materialised reply (SIMP-019), not true provider-native token streaming. That arrives with the durable yield model at Gate 2. Full integration story + Open WebUI setup steps: [`docs/streaming-and-openai-shim.md`](docs/streaming-and-openai-shim.md).

### M9 — tool node + first external action (working today)

```powershell
# Windows: brings up memory + ai + tool + bridge, parks until Ctrl-C.
.\scripts\relix-mesh-up.ps1
```

The tool node registers `tool.web_fetch` (HTTPS GET, UTF-8 bodies, body-cap
+ deadline + redirect-cap). It runs the same admission pipeline as every
other peer — identity → policy → handler → audit — and adds an
**SSRF guard** that rejects loopback, RFC 1918, link-local (incl. cloud
metadata endpoints), ULA, multicast, broadcast, documentation / benchmark
ranges, IPv4-mapped IPv6, `file://`, `ftp://`, and any non-http(s) scheme
**before** the network call. Failures surface as `policy_denied` so the
bridge returns 502 with the exact reason.

Two ways to drive the tool flow (both run `flows/chat_with_tool.sol` —
SOL owns the orchestration, the bridge only selects the template):

```powershell
# Native endpoint
Invoke-RestMethod -Method Post http://127.0.0.1:19791/chat_with_tool `
    -ContentType 'application/json' `
    -Body (@{ session_id='demo'; message='summarize this'; url='https://example.com/' } | ConvertTo-Json)

# OpenAI shim auto-routes to the tool flow when the user message contains an http(s) URL.
Invoke-RestMethod -Method Post http://127.0.0.1:19791/v1/chat/completions `
    -ContentType 'application/json' `
    -Body (@{ model='relix-mock'; messages=@(@{role='user';content='Please fetch https://example.com/ and summarize.'}) } | ConvertTo-Json)
```

The tool node also **pins** the outbound connection to the IPs the SSRF
guard validated (via `reqwest::ClientBuilder::resolve_to_addrs`), so the
TCP connect cannot diverge from the inspected address (DNS-rebind window
closed). `Host` header + TLS SNI keep targeting the original hostname.
**Every redirect target** is re-screened by a
`reqwest::redirect::Policy::custom` closure that runs the SSRF guard
again before the follow — cross-hostname `Location:` hops to
loopback / RFC 1918 / link-local / metadata are rejected pre-connect.
See SIMP-021 and
[`docs/tool-node-security.md`](docs/tool-node-security.md).

### M10 — runtime capability discovery (working today)

Every controller now serves a built-in `node.manifest` capability that
returns its current `NodeManifest` (node id, type, listen endpoints, and
the live set of `CapabilityDescriptor`s). The web bridge runs a one-shot
**discovery pass** at startup: dials each peer in `peers.toml`, pulls
their manifests, and caches them.

Two operator-visible effects:

1. SOL flows can target a **capability** instead of a hard-coded peer
   alias:
   ```text
   remote_call("capability:tool.web_fetch", "tool.web_fetch", url);
   ```
   The dispatcher resolves the prefix to whichever cached peer
   advertises the method. Static aliases (`"memory"`, `"ai"`, etc.)
   continue to work unchanged — `flows/chat.sol` and the other M5–M9
   flows did not need updating.
2. `GET /v1/models` derives extra entries from the cache. Any peer that
   advertises `ai.chat` shows up as `relix-<provider>` (provider name
   carried in the descriptor's `sensitivity_tags`). Operator-curated
   entries from `[openai_compat] models = [...]` still win on id
   collisions.

Discovery is best-effort: a failed pull leaves that peer absent from the
cache, the bridge logs a warning, and static aliases keep working. The
manifest payload is *not* signed in the alpha (that lands at Gate 2 along
with full gossip-based propagation).

### M11 — connection pooling (working today)

Pre-M11, every `/chat` request brought up a fresh libp2p swarm, dialled
all peers, performed TCP + Noise + Yamux handshakes, then dropped the
transport on completion. Logs showed N+1 `transport listening` lines for
N requests.

M11 lifts the discovery transport into a long-lived `MeshClient` that is
stored in `AppState` and reused for every chat. `FlowRunner` now takes
`Option<Arc<MeshClient>>`; when set, it skips its own transport setup.
The standalone `relix-cli flow-run` path (no bridge) keeps the original
per-call transport — the option is `None` for that caller.

The same pattern, with stricter keying, applies to the tool node's HTTP
client: a `PinnedClientPool` caches `reqwest::Client`s keyed by
`(hostname, sorted_validated_addrs)` so repeat fetches reuse TLS +
hyper connection state without ever sharing a `Client` whose pin doesn't
match the request's validated route. Live measurement: cold first fetch
**229 ms**, warm steady **~90 ms** (~60% reduction) with every SSRF,
DNS-pin, and redirect invariant intact. Details:
[`docs/tool-node-security.md`](docs/tool-node-security.md) §"Secure
client pool".

Local benchmark on a clean mesh (mock provider, 10 sequential `POST /chat`
calls, warm cache):

```
samples ms : 106, 51, 50, 50, 49, 49, 49, 49, 50, 50
min  : 49 ms     avg : 55 ms     p50 : 50 ms
transport listening lines in bridge.log : 1   (was N+1 = 11 pre-M11)
```

The first request is slightly slower because it warms up libp2p's
request-response state for each peer; from request 2 onward the path is
steady-state. Workload-realistic numbers (real provider, larger bodies)
will look different — this is just a baseline confirming the pool reuses
the same connection.

Full bringup (tool / web nodes — M7+ continuing work): see `ops/runbooks/alpha-bringup.md`.

## Reporting Security Issues

See [`SECURITY.md`](SECURITY.md).

## License

Apache License 2.0. See [`LICENSE`](LICENSE).

## Minimum Supported Rust Version

Rust 1.95 (pinned in `rust-toolchain.toml`). Earlier versions are not tested.
