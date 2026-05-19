# Relix Broad Alpha — Plan of Record

This is the plan the alpha is being built against. The week-by-week structure was developed during architecture review; this document is the persisted version. The plan is firm; alpha scope is bounded by it.

## Goals

A one-week broad alpha that exercises every major Relix platform piece in concert:

- Real P2P (separate OS processes, libp2p inherited from OpenPrem `network/rpc.rs`).
- Signed identities (`IdentityBundle`).
- Group-based policy on every responder.
- Hash-chained event log + per-responder audit.
- Capability registry with on-connect manifest exchange.
- SOL flows as the only routing surface.
- Memory node (SQLite + FTS5, Hermes-inspired schema).
- AI node (Anthropic, streaming via SSE).
- Tool node (`tool.web_fetch` with URL allowlist).
- Web bridge (HTTP/SSE) feeding Relix Web (Open WebUI fork).
- One canonical chat flow (`flows/chat.sol`) plus a tool-aware variant.

## Non-Goals (Will Not Be Built)

- Marketplace.
- Central gateway.
- Credentials in the web backend.
- Routing decisions outside SOL.
- HSM, IA hierarchy, federation, SolFlow live mode, mobile peers, voice, image generation, general MCP, dynamic tool discovery.

## Architecture Invariants

Carried into the alpha unchanged from the substrate freeze:

1. The responding node enforces. Identity, policy, and audit live on the side of the data, never centralized.
2. AI provider keys live ONLY in the AI node's local config.
3. The web backend in `RELIX_MODE` does not call any LLM provider.
4. Adding a future channel node (Telegram, Slack) requires zero changes to memory/AI/tool/web nodes — only a new binary and a new SOL flow.

## Daily Plan

### Day 1 — Foundation, Two-Node P2P, Signed Identity
Build: workspace; reuse OpenPrem RPC + SOL VM verbatim; add `relix-core` (codec, types, bundle, identity, policy, eventlog) skeleton; controller binary boots, generates identity, listens; `relix-cli ping` works.
Demo: two controllers exchange a signed RPC; invalid identity rejected.

### Day 2 — SOL Cross-Node + Memory Node
Build: extend SOL VM with `RemoteCall` opcode + dispatcher callback; capability registry; memory node (Hermes FTS5 schema) registering `memory.search`, `memory.write_turn`, `memory.recent_for_session`; first SOL flow.
Demo: SOL flow on controller A invokes memory node B's capability; flow log shows issued+completed events.

**Status:** SOL `RemoteCall` opcode + dispatcher + flow-log tracing landed in M6/S4. `flows/ping.sol` is the M6 single-peer demo; `flows/chained_health.sol` is the M6/S7 multi-peer demo. M7 ships memory (SQLite + FTS5) and the conversational state-machine flow `flows/chat.sol`. M7+M8a make AI provider-agnostic via the `ChatProvider` trait: `mock` / `openai` / `openrouter` / `xai` / `local` / `anthropic` / `gemini` (stub) — see `docs/provider-configuration.md`. M8 ships `relix-web-bridge`: `POST /chat` JSON → SOL flow → audited mesh round-trip → JSON reply. Bridge is a normal peer with its own identity; owns no provider key. **M8/S2** adds `POST /chat/stream` (Relix SSE), `POST /v1/chat/completions` + `GET /v1/models` (OpenAI-compatible shim, both streaming and non-streaming), and an Open WebUI integration path that requires no fork — see `docs/streaming-and-openai-shim.md`. Streaming is bridge-level (SIMP-019); true per-token streaming and Cedar policy land at Gate 2. Next: tool node (`tool.web_fetch` with URL allowlist) and `flows/chat_with_tool.sol`.

### Day 3 — AI Node + Streaming + Chat Flow
Build: streaming substream protocol (simplified); AI node wrapping Anthropic with token stream; `flows/chat.sol`; web bridge HTTP server with SSE.
Demo: `curl` against the bridge produces a real streamed Anthropic response routed via SOL.

### Day 4 — Policy + Identity Enforcement
Build: allowlist policy engine; wire into admission pipeline step 9; node-local policy bundles signed by org root; audit records include policy decisions.
Demo: same chat flow with policy denying unauthorized identity; audit shows the decision and matched rule.

### Day 5 — Relix Web
Build: fork Open WebUI into `relix-web/`; strip provider plumbing under `RELIX_MODE=true`; add `relix_provider.py` that POSTs to the local web bridge; verify no LLM provider calls from web backend.
Demo: browser-based chat through Relix Web; full audit trail visible.

### Day 6 — Tool Node
Build: tool node with `tool.web_fetch` (URL allowlist); `flows/chat_with_tool.sol`; alpha tool-marker convention; policy requires `tool-users` group.
Demo: "Fetch example.com and summarize" works end-to-end with tool node audit.

### Day 7 — Integration, Replay Smoke, Demo Script
Build: `relix-flow-inspect --replay-verify`; crash-recovery smoke; operational tooling polish; runbooks; alpha demo script.
Demo: end-of-week demo per `docs/demo-script.md`.

## Acceptance Criteria

Documented in `ops/runbooks/alpha-bringup.md` (smoke section). The acceptance set is fixed; the alpha is not done if any item fails:

1. Four `relix-controller` instances run as separate OS processes and discover each other.
2. Every cross-node RPC is identity-verified on the responder before any handler logic.
3. Every cross-node RPC produces an audit record on the responder, correlatable across nodes by `request_id`.
4. `chat.sol` works end-to-end from Relix Web with memory persistence across sessions and streamed tokens.
5. `chat_with_tool.sol` works end-to-end with a real `web.fetch` call.
6. Anthropic API key present ONLY in AI node config.
7. Web backend makes no LLM provider calls in `RELIX_MODE`.
8. No routing decision encoded outside SOL.
9. Replay-verify reports integrity OK on a recorded flow log.
10. Killing any single node does not crash the mesh.
11. Documentation present and accurate (`docs/`, `ops/runbooks/`, `specs/alpha-simplifications.md`).
12. CI runs `cargo test --workspace` and the integration demo job, both pass.
13. No marketplace code.
14. No secrets in the repository.

## Risk Register

- libp2p first-build compile time (one-time pain, accept).
- Open WebUI fork friction (mitigate: `RELIX_MODE` flag, minimal changes).
- Synchronous SOL execution (documented; Gate 2 addresses with yield model).
- Streaming under restart fragility (documented; alpha demo does not test cross-restart streaming).
- Anthropic rate-limit during demo (mitigate: cached fallback response).

## Status

Active. Daily updates in commits.
