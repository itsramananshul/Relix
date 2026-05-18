# Code Reuse Map

This document records every external code asset Relix builds on, with a per-asset disposition (**reuse**, **adapt**, **reference**, **discard**) and a target location inside this repository. Relix is built **on top of** OpenPrem; it is not a clean-room rewrite.

The reuse rule: prefer adapting an existing OpenPrem primitive to reinventing it. Reinvention requires explicit justification in the relevant PR description and an entry in `specs/alpha-simplifications.md`.

## OpenPrem INFRA (`D:\DATA\WORK\OpenPrem\Apps\INFRA\open-prem-main`)

| Path | Purpose | Disposition | Target Relix Location | Notes / Risk |
|---|---|---|---|---|
| `src/network/rpc.rs` (357 LOC) | libp2p RPC: TCP + Noise XK + Yamux + CBOR `request_response` + Kademlia. `Client`, `Responder`, `EventLoop`, `Behaviour`. | **Reuse directly** | `crates/relix-runtime/src/transport/rpc.rs` | Production-grade. Re-export `PeerId`, `Multiaddr`. Wraps everything Relix needs at the transport layer. Note: first-time libp2p compile is slow (~minutes); accept once. |
| `src/network/network.rs` (190 LOC) | App-level wrapper: `NodeConfig`, `PeerStore`, `init()` that dials peers and bootstraps Kademlia, optional ping. | **Reuse + adapt** | `crates/relix-runtime/src/transport/network.rs` | Keep the alias/peer-store mechanic for the alpha's dev-time peer discovery. Production replaces `network.toml` with signed bootstrap list. |
| `src/sol/lexer.rs` (383 LOC) | SOL tokenizer. | **Reuse directly** | `crates/relix-runtime/src/sol/lexer.rs` | No changes. |
| `src/sol/parser.rs` (707 LOC) | Recursive-descent parser producing `Program` AST; supports imports, structs, enums, fns. | **Reuse + extend** | `crates/relix-runtime/src/sol/parser.rs` | Imports already parse; we add `remote_call(target, "method", args)` as a callable surface (no new syntax — call site looks like a function call). |
| `src/sol/analyzer.rs` (457 LOC) | Semantic analysis + type checking. | **Reuse directly** | `crates/relix-runtime/src/sol/analyzer.rs` | No changes for alpha. |
| `src/sol/bytecode.rs` (641 LOC) | Codegen; `enum Inst { ... }` with arithmetic, control flow, heap ops, calls. | **Reuse + extend** | `crates/relix-runtime/src/sol/bytecode.rs` | Add `Inst::RemoteCall { peer_idx, method_idx }` and `Inst::StreamNext { handle_slot }` per alpha simplification (synchronous). String pool indices added if needed. |
| `src/sol/vm.rs` (318 LOC) | Stack-based VM with `step()` and `run()`. | **Reuse + extend** | `crates/relix-runtime/src/sol/vm.rs` | Add dispatcher callback for `RemoteCall` opcodes. The existing `step()` design makes yield-based suspension straightforward when needed at Gate 2. |
| `src/sol/init.rs` (25 LOC) | Pipeline: source → lexer → parser → analyzer → codegen → VM. | **Reuse** | `crates/relix-runtime/src/sol/init.rs` | Unchanged. |
| `src/sol/cli.rs`, `src/sol/main.rs`, `src/sol/mod.rs`, `src/sol/util.rs` | SOL standalone CLI + module wiring. | **Reuse** | `crates/relix-runtime/src/sol/` | Keep as-is; useful for SOL-only debugging. |
| `src/handler.rs` (12-line stub) | Placeholder `Handler::new()` / `start()`. | **Replace** | `crates/relix-runtime/src/dispatch.rs` (new) | Real dispatch bridge: inbound RPC method → SOL session OR native capability handler. This is the documented "missing link" from `openprem-full-context.md`. |
| `src/session.rs` (62 LOC) | Stub `Session` / `SessionManager` with placeholder `Inst {}` and unused fields. | **Discard** | n/a | Predates the SOL VM split. Replaced by the dispatch bridge + flow coordinator. |
| `src/init.rs` (118 LOC) | `Controller` struct, config parsing, SOL session loading. | **Reuse + extend** | `crates/relix-runtime/src/controller.rs` | Add: identity loading, policy bundle loading, capability registration, transport startup. Keep config-shape layout (TOML) for continuity. |
| `src/main.rs` (53 LOC) | Builds Controller, prints, exits. | **Reuse skeleton** | `crates/relix-controller/src/main.rs` | Rewrite to actually run: spawn transport, register capabilities, enter event loop. |
| `Cargo.toml` | Edition 2024, libp2p 0.54, tokio, serde, toml. | **Reuse dep set** | workspace + `relix-runtime` Cargo.toml | Pin same versions to avoid integration surprises. |
| `NETWORK.md`, `README.md`, `config.toml`, `network.toml` | Network docs + example configs. | **Reference** | `docs/openprem-network-reference.md` (excerpt), `configs/` (templates) | Templates inform the alpha config files. |
| `tests/*.sol` (18 SOL test scripts) | SOL regression tests. | **Reuse** | `crates/relix-runtime/tests/sol/` | Keep all SOL tests; ensure they still pass after extension. |

## OpenPrem SolFlow (`D:\DATA\WORK\OpenPrem\Apps\SolFlow`)

| Path | Purpose | Disposition | Target |
|---|---|---|---|
| `src/emit/emit.ts` | Graph → SOL text exporter (Phase A). | **Reference only** | Not part of alpha — SOL flows are hand-written. Re-engage at Gate 2 / SolFlow live mode. |
| `src/graph/`, `src/components/`, `src/stores/` | Vue 3 + Vue Flow editor. | **Reference only** | Same — alpha does not include SolFlow. |
| `src/runtime/interpret.ts`, `simulate.ts` | TS-side flow simulation for the editor. | **Reference only** | Not used by the Relix runtime. |
| `api/sol-man/` | Future SolMan AI assistant code. | **Discard for alpha** | Out of scope. |

## Reference Material in `Apps/Relix/reference/`

| Asset | Used For | Used Where |
|---|---|---|
| `openclaw-main` | Plugin manifest shape (`openclaw.plugin.json`), channel-adapter conventions. | `crates/relix-runtime/src/capability/manifest.rs` (inspired-by, not ported). Future channel nodes will mirror adapter conventions. |
| `hermes-agent-main` | Session storage (`hermes_state.py` — SQLite + FTS5 schema), tool registry pattern. | `crates/relix-node-memory/` (schema ported to `rusqlite`). Tool registry pattern noted; not directly ported (in Relix a tool is a node). |
| `open-webui-main` | The chat UI, auth, persistence, `routers/openai.py` provider seam. | `relix-web/` (forked subset; provider plumbing stripped and replaced with `RelixProvider` that POSTs to the local bridge). |

Reference folders are excluded from git via `.gitignore`. They remain on disk for engineer reference but are not part of the build.

## OpenPrem Workspace Apps NOT Reused

| Path | Why Not |
|---|---|
| `Apps/DEMO/*` | Demo apps (app-nexus, product-inventory, etc.). Separate products; not substrate. |
| `Apps/Voxvitals/` | Separate app. Not relevant to runtime. |
| `Apps/n8n/` | Vendored n8n. Reference value only; reviewed during architecture analysis, not reused. |
| `Apps/product-inventory/` | Demo Next.js app. Not relevant. |

## What's Missing — The Gap Relix Fills

The reuse audit confirms OpenPrem provides: transport, language, VM. Relix adds:

1. **Dispatch bridge** (replaces `handler.rs` stub): method name → SOL session OR native handler.
2. **Identity verification pipeline** + signed `IdentityBundle` envelope.
3. **Policy engine** (allowlist DSL for alpha; Cedar at Gate 2).
4. **Capability registry** + on-connect manifest exchange.
5. **Event log** (append-only, hash-chained, signed) per flow + audit indexing.
6. **CBOR codec** with deterministic encoding (sits beneath identity/audit/capability bundles).
7. **`remote_call` opcode** + dispatcher callback in the SOL VM.
8. **Coordinator** (minimal — owns per-flow logs and outbound call accounting).
9. **Node implementations**: memory (SQLite+FTS5 from Hermes), AI (Anthropic), tool (web.fetch).
10. **Web bridge** (HTTP/SSE) and the **Open WebUI fork** as Relix Web.

Each item maps to a crate or module documented elsewhere in `docs/` and `specs/`.

## Risk Register

- **libp2p 0.54 compile time** on first build (~minutes). Acceptable one-time cost; incremental builds are fast.
- **OpenPrem session.rs is unused legacy** — discarding it does not break anything (verified by reading `main.rs` and `init.rs`).
- **SOL VM expects synchronous execution** — the alpha's `remote_call` blocks the VM thread. This is documented in `specs/alpha-simplifications.md`. Gate 2 introduces yield-based suspension; the `step()` API already supports it.
- **No replay-equivalence property test framework yet** — alpha ships partial integrity verification (`relix-flow-inspect --replay-verify`) only. Full property is Gate 2.
