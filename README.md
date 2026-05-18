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

The AI node selects its provider via `[ai] provider = "mock"` (default; deterministic; no secrets) or `"anthropic"` (real Claude via `reqwest`; API key from `$ANTHROPIC_API_KEY` or the gitignored `api_key_path` file). The SOL flow is identical for both — adding a new provider is an `impl ChatProvider` + a `build_provider` arm, not an architectural change.

Full bringup (tool / web nodes — M7+ continuing work): see `ops/runbooks/alpha-bringup.md`.

## Reporting Security Issues

See [`SECURITY.md`](SECURITY.md).

## License

Apache License 2.0. See [`LICENSE`](LICENSE).

## Minimum Supported Rust Version

Rust 1.95 (pinned in `rust-toolchain.toml`). Earlier versions are not tested.
