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

## Quickstart

See `ops/runbooks/alpha-bringup.md`.

## Reporting Security Issues

See [`SECURITY.md`](SECURITY.md).
