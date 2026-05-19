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
- **Tool node** (`tool.web_fetch`) with URL allowlist and tool-users group requirement.
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

Full bringup (tool / web nodes — M7+ continuing work): see `ops/runbooks/alpha-bringup.md`.

## Reporting Security Issues

See [`SECURITY.md`](SECURITY.md).

## License

Apache License 2.0. See [`LICENSE`](LICENSE).

## Minimum Supported Rust Version

Rust 1.95 (pinned in `rust-toolchain.toml`). Earlier versions are not tested.
