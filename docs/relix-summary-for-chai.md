# Relix — Full Current State (Summary for Chai / ChatGPT)

Send this to ChatGPT before asking for feature prompts.
This document is the ground truth. Every file path, crate name,
and design decision must match what's written here.
Last updated: 2026-05-21.

---

## What Relix Is

Relix is a **production-grade decentralized AI agent platform**.

Key difference from everything else: **no central gateway**. Every node is a peer.
There is no single server the whole system depends on. Every node enforces its own
security — identity verification, policy, and audit happen on the node receiving
the request, not on a hub.

Built for: enterprises, multi-org deployments, external operators, production workloads.
NOT a research project. NOT an alpha toy.

---

## The Stack

| Layer | Technology |
|-------|-----------|
| Transport | libp2p, TCP, Noise XK encryption, Yamux multiplexing |
| RPC | CBOR-encoded, custom `/relix/rpc/1` protocol |
| Peer discovery | Kademlia DHT (automatic — no static address books needed) |
| Orchestration | SOL — a domain-specific scripting language built by us |
| Identity | Ed25519 key pairs, CBOR-encoded IdentityBundle, org root signing |
| Policy | Allowlist engine (Cedar upgrade is next) — deterministic, auditable |
| Audit | Hash-chained append-only event log on every node |
| Storage | SQLite, WAL mode, dual FTS5 index |

---

## Architecture — The 5 Rules That Never Break

Every feature, every prompt, every design decision must respect these:

1. **The responding node enforces.** Identity → policy → handler → audit runs on the node RECEIVING the request. Never centralized.
2. **AI keys live only in the AI node.** No other node ever sees an LLM API key. Not the web backend. Not the coordinator. Nobody else.
3. **Web backend makes zero LLM calls.** In RELIX_MODE it only proxies to the bridge.
4. **No routing outside SOL.** All multi-node orchestration is expressed in SOL flows. Never hardcode routing logic in Rust.
5. **New channels = zero changes to existing nodes.** Adding Telegram, Slack, Discord only requires a new binary + a new SOL flow. Memory/AI/tool nodes don't change.

---

## Codebase Structure

Root: `D:\DATA\WORK\OpenPrem\Apps\Relix\`

### Rust Crates

| Crate | What it does | Lines |
|-------|-------------|-------|
| `crates/relix-core` | Types, codec (CBOR), IdentityBundle, PolicyEngine, EventLog, CapabilityDescriptor, audit, router wire types | 3,493 |
| `crates/relix-controller` | Binary — boots a node, reads config, starts the runtime | 25 |
| `crates/relix-runtime` | SOL VM, FlowRunner, dispatch pipeline, all node implementations, libp2p transport | 43,167 |
| `crates/relix-web-bridge` | HTTP/SSE server that is itself a peer; translates HTTP ↔ libp2p RPC | 15,590 |
| `crates/relix-cli` | CLI: identity, task, capability, flow-run, router, ops, sol, doctor, browser, mcp, fs, terminal, web subcommands | 6,963 |
| `crates/relix-flow-inspect` | Operator binary: reads audit + flow logs, replay-verify | 197 |
| `crates/relix-telegram` | Telegram channel (scaffold complete; needs bot token) | 922 |

**Total: ~70,362 lines of Rust. 1,257 tests passing.**

### Node Types (all same binary, different config)

| Node | Capabilities |
|------|-------------|
| memory | `memory.write_turn`, `memory.recent_for_session`, `memory.search` |
| ai | `ai.chat` (streaming, provider-agnostic: mock/openai-compat/anthropic/gemini/openrouter/xai/local) |
| tool | filesystem (12), web (7), terminal (10), browser (9), MCP (3), PDF+text (2), plus universal |
| coordinator | Full task ledger — 35+ `task.*` capabilities |
| web-bridge | HTTP/SSE/OpenAI-shim gateway — 65 routes |
| router | `router.heartbeat`, `router.network_summary`, `router.session_list`, `router.log` — **BUILT AND LIVE** |

### SOL Flows (`flows/`)

SOL is the ONLY place routing decisions live:
- `chat.sol` — full conversational chat with memory
- `chat_with_tool.sol` — chat + tool node invocation
- `ping.sol`, `chained_health.sol`, `memory_demo.sol`

### Config files (`configs/`)

Every node type has a `.toml` config. The `role` field in `[controller]` determines what the node does:
- `role = "controller"` (default) — normal execution node
- `role = "router"` — network observability node (LIVE)
- `configs/router-node.toml` — example router config
- `configs/policies/router.toml` — Cedar policy for router

---

## What's Already Built — Do Not Re-Build

### Foundation (always was here)
- Full libp2p transport, Noise XK, Yamux
- Ed25519 identity, IdentityBundle, org root signing
- Allowlist policy engine (Cedar is Gate 2 upgrade)
- Hash-chained audit log + per-flow event log
- All 5 original node types (memory, ai, tool, coordinator, web-bridge)
- Task system: 8-state lifecycle, attempts/lineage, recovery, retry, export
- Scale-grade event system: cursor pagination, typed envelopes, SSE, retention design
- Full `/v1` HTTP API surface on the web-bridge
- CLI: identity, task, capability subcommands
- SOL VM: parser, bytecode, analyzer, dispatcher, remote_call

### Wave 1 — CLOSED (all live)
- **Router node** (`crates/relix-core/src/router.rs` + `crates/relix-runtime/src/nodes/router.rs`): heartbeat collection (60s), stale-peer reaping (90s threshold), network summary, session tracking, log aggregation — **fully live, 12 tests**
- **Browser backends**: `HeadlessChromeBackend`, `PlaywrightBackend`, `WebDriverBackend` — all live behind Cargo features (`browser-headless-chrome` / `-playwright` / `-webdriver` / `browser-all`). Operator picks via `[tool.browser] backend = "..."`. Lazy launch, fails loud if feature not compiled.
- **MCP stdio runtime**: `McpStdioClient` in `crates/relix-runtime/src/nodes/tool/mcp_stdio.rs` — live subprocess spawn, mutex-serialised JSON-RPC. `tool.mcp.invoke` + `tool.mcp.list_tools` route through live client for stdio transport.
- **Terminal PTY**: full terminal suite (run/spawn/cancel/sessions/tail/audit/shell.{open,input,close,control}) — all live. PTY backend via `portable-pty` behind `--features terminal-pty`.
- **Filesystem parity**: read/write/append/list_dir/patch/patch_preview/binary_sniff/audit_recent/search_files(glob)/fuzzy_replace/tree/stat — all live.
- **Web parity**: web_fetch/web_get/web_search/web_extract(markdown mode)/web.post/web.robots_check/web.blocklist_summary — all live.
- **PDF + text chunking**: `tool.pdf`, `tool.text.chunk` — live.
- **RiskLevel on CapabilityDescriptor**: every capability has explicit risk tier (Safe/Low/Medium/High/Critical).
- **Operator blocklist**: `[tool] blocked_hosts` in config — runs before scheme/DNS and on every redirect.

### Wave 2 — IN PROGRESS (most items closed)
- **Dispatch observability** (CLOSED): `node.dispatch.stats`, `GET /v1/dispatch/stats`, `relix-cli ops dispatch-stats`. Per-capability latency (last/max/mean/samples) lives in memory. No dedicated dashboard page.
- **Policy hardening** (CLOSED): `node.policy.simulate` (what-if check), `node.policy.recent_denials` (denial ring), both with bridge proxies and CLI (`relix ops`). The denial ring surfaces in the Policy Denials panel (`/v1/policy/denials`); simulate has no dedicated panel.
- **Task replay** (substantively CLOSED): `task.replay` clones task with `retried_from` edge, `POST /v1/tasks/:id/replay`, Replay button on dashboard, per-step durations in timeline.
- **Health-aware AI router**: `HealthAwareRouter` filters cooldown/quarantined providers and ranks by success_ratio. `NoopRouter` + `ProviderRouter` trait as the foundation. `POST /v1/providers/route_test` lets operators preview router decisions.
- **Anthropic prompt caching + extended thinking**: `cache_control: ephemeral` on system block; opt-in `thinking_budget_tokens` on ChatInput.
- **SOL quick-add** (started): `relix-cli sol templates` + `sol new --template <name> --out <path>` — 6 baked-in workflow templates.
- **Doctor CLI** (started): `relix-cli doctor` — bridge health probe, PASS/WARN/FAIL report, non-zero exit on FAIL for CI.

---

## Complete Capability Index

### Universal (every node)
- `node.health`, `node.manifest`
- `node.dispatch.stats` — per-capability latency snapshot
- `node.policy.simulate` — what-if policy check without invoking
- `node.policy.recent_denials` — bounded denial ring

### Memory node
- `memory.write_turn`, `memory.recent_for_session`, `memory.search`

### AI node
- `ai.chat` — provider-agnostic streaming chat (mock/openai-compat/anthropic/gemini/openrouter/xai/local/ollama)

### Router node (`role = "router"` in config)
- `router.heartbeat` — controller push every 60s; registers peer + caps + groups
- `router.network_summary` — operator mesh overview (peers, health, sessions, uptime, org_filter)
- `router.session_list` — operator session browser (status_filter, limit, offset)
- `router.log` — controller push; bounded 10k-line in-memory ring

### Coordinator node
Core lifecycle: `task.create`, `task.update`, `task.get`, `task.list`, `task.cursor`, `task.count`
Chronicle: `task.events`, `task.recent_events`, `task.export`, `task.compact_events`
Graph: `task.attempts`, `task.edges`, `task.recent_edges`, `task.lineage`, `task.subtree_metrics`
Cross-task: `task.spawned_child`, `task.delegated_to`, `task.awaiting`
Interruption: `task.pause_requested`, `task.resume_requested`, `task.freeze_requested`, `task.unfreeze_requested`, `task.pause_observed`, `task.resume_observed`, `task.freeze_propagated`
Recovery: `task.retry`, `task.recover`, `task.replay`, `task.stuck`, `task.attempt_orphan_closed`
Operator: `task.note`, `task.mark_investigation`, `task.transition_check`, `task.terminal_summary`, `task.thrash_detected`, `task.retry_exhausted`
Todo: `task.todo_set`, `task.todo_list`, `task.todo_update`

### Tool node
**Filesystem** (jailed): `tool.read_file`, `tool.write_file`, `tool.append_file`, `tool.list_dir`, `tool.search_files`, `tool.patch`, `tool.patch_preview`, `tool.binary_sniff`, `tool.fs.audit_recent`, `tool.fuzzy_replace`, `tool.fs.tree`, `tool.fs.stat`

**Web**: `tool.web_fetch`, `tool.web_get`, `tool.web_search`, `tool.web_extract` (modes: text/title/links/meta/markdown/all), `tool.web.post`, `tool.web.robots_check`, `tool.web.blocklist_summary`

**Terminal**: `tool.terminal.run`, `tool.terminal.spawn`, `tool.terminal.sessions`, `tool.terminal.cancel`, `tool.terminal.tail`, `tool.terminal.audit_recent`, `tool.terminal.shell.open`, `tool.terminal.shell.input`, `tool.terminal.shell.close`, `tool.terminal.shell.control`

**Browser** (feature-gated backends — HC/PW/WD): `tool.browser.open_session`, `tool.browser.close_session`, `tool.browser.navigate`, `tool.browser.get_text`, `tool.browser.screenshot`, `tool.browser.list_sessions`, `tool.browser.click`, `tool.browser.type_text`, `tool.browser.wait_for_selector`

**MCP**: `tool.mcp.list_servers`, `tool.mcp.list_tools`, `tool.mcp.invoke`

**PDF + text**: `tool.pdf`, `tool.text.chunk`

---

## HTTP Endpoints (web-bridge — 65 routes)

**Tasks**: GET/POST `/v1/tasks`, `count`, `cursor`, `:id`, `:id/attempts`, `:id/edges`, `:id/lineage`, `edges/recent`, `events/recent`, `events/stream`, `stuck`, `:id/todos` (GET/PUT/PATCH), `:id/summary`, `:id/events`, `:id/events/stream`, `recover`, `:id/retry`, `:id/replay`, `:id/cancel`, `:id/note`, `:id/investigation`, `:id/pause`, `:id/resume`, `:id/freeze`, `:id/unfreeze`

**Capabilities**: `GET /v1/capabilities`, `/v1/capabilities/:method`

**Topology**: `GET /v1/topology`, `/v1/topology/events`, `/v1/streams`, `/v1/routing`

**AI/Chat**: `POST /chat`, `/chat/stream`, `/chat_with_tool`, `/v1/chat/completions` (OpenAI shim), `GET /v1/models`

**MCP**: `GET /v1/mcp/servers`, `/v1/mcp/tools`, `/v1/mcp/audit`, `POST /v1/mcp/invoke`

**Observability**: `GET /v1/fs/audit`, `/v1/terminal/audit`, `/v1/browser/sessions`, `/v1/dispatch/stats`, `/v1/tool/blocklist`

**Policy**: `GET /v1/policy/simulate`, `/v1/policy/denials`

**Providers**: `GET /v1/providers/health`, `POST /v1/providers/route_test`, `GET/PUT/DELETE /v1/config/providers/:name`, `GET/PUT /v1/config/telegram`, `POST /v1/config/telegram/test`

**Config**: `GET /v1/config`, `/v1/health`, `/health`

---

## CLI Surface

| Command | What it does |
|---------|-------------|
| `relix-cli identity` | Mint / inspect identity bundles |
| `relix-cli task` | create/get/list/count/update/retry/cancel/pause/resume/freeze/unfreeze/note/investigate/watch/export/todo |
| `relix-cli capability ls [--risk tier]` | Per-peer manifest dump; risk-tier filter |
| `relix-cli router status/peers/sessions` | Router mesh overview via libp2p dial |
| `relix-cli ops providers-health/capabilities/stuck/events/route-test/dispatch-stats` | Operator observability |
| `relix-cli sol templates / sol new` | List + quick-generate SOL flow files from templates |
| `relix-cli doctor` | Bridge health probe — PASS/WARN/FAIL |
| `relix-cli flow-run` | Execute a SOL flow |
| `relix-cli ping` | Direct libp2p peer ping |
| `relix-cli topology show/health` | Topology + bridge health |
| `relix-cli mcp servers/tools/audit` | MCP registry inspection |
| `relix-cli fs audit` | Filesystem mutation ring mirror |
| `relix-cli terminal sessions/audit/cancel` | Terminal observability |
| `relix-cli browser sessions` | Browser session list |
| `relix-cli web blocklist` | Host blocklist snapshot |

---

## Dashboard Panels

Since the v0.3.0 rebuild the console is a single-page app: a sidebar
of panels, selected by click, with no `#/...` hash routes. The
current build has twenty-two. The `SECTIONS` array in
`crates/relix-web-bridge/src/dashboard.html` is the source of truth.

| Panel | What it shows |
|------|--------------|
| Overview | KPI grid, System Health (rolls up `/v1/topology` + per-agent scores), Recent Activity |
| Tasks | Task-ledger summary + table with status filter, search, and Spawn Task (`/v1/tasks`) |
| Scheduled Jobs | Cron job table with subject filter, New Job, and trigger (`/v1/cron/jobs`) |
| Chat | Send a message through a provider and read the reply + stats |
| Memory | Search / ingest / inspector / dialectic over the memory store |
| Approvals | Pending / history / failed-delivery / channels |
| Skills | Skill catalogue + statistics |
| Sessions | Recent sessions + content search |
| Reasoning | Smart routing, self-consistency, belief state, judge verdicts |
| Credentials | Vault, rotation schedule, per-credential audit log |
| Identity | Active session tokens + research identity |
| Cost & Metrics | Cost by provider/agent, 24h trend, baselines, alerts, spend caps |
| Observability | OTel/sink status, per-agent health, session debugger, provenance, alerts |
| Policy Denials | Recent admission denials with peer filter (`/v1/policy/denials`) |
| Multi-Tenant | Tenant list + per-tenant detail |
| Planning | Create/inspect plans (planner + critic) |
| Workflows | Active + registered workflows |
| Email | SMTP/IMAP status + recent inbound messages |
| Plugins | Installed subprocess plugins |
| MCP Servers | Registered MCP servers with peer filter, tool listing, and invoke (`/v1/mcp/servers`) |
| Configuration | Providers, routing tiers, effective (redacted) config |
| Logs | Live `/v1/logs/stream` tail with filters |

There is no Topology, Capabilities, Metrics, fsaudit, termaudit,
browser, or Telegram page. Capability, topology, filesystem-audit,
terminal-audit, and browser-session data are on the HTTP API and the
`relix` CLI; provider config is in the Configuration panel.

---

## Known Stubs / Honest Gaps

1. **`relix-telegram`** — scaffold complete, no live HTTPS BotApi HTTP client yet.
2. **`GeminiProvider`** — 53-line stub (`crates/relix-runtime/src/nodes/ai/provider/gemini.rs`). Returns error on call.
3. **Chronicle compaction Step 3** — `task.compact_events` does dry-run only. Full SQLite rewrite deferred.
4. **Resumable SOL VM** — deferred to Gate 2. VM runs to completion; no mid-flow checkpoint.
5. **Browser navigate/get_text/screenshot** — live only with feature-flagged backends compiled in. Default build returns `BackendNotConnected`.
6. **`tool.mcp.invoke` HTTP transport** — returns `RuntimeNotConnected` for HTTP MCP servers; stdio is live.
7. **Filesystem delete/mkdir/move/rename/copy** — not surfaced; policy decision pending.

---

## Feature Backlog (Priority Order)

### Cedar Policy Engine
Replace the current allowlist engine in `crates/relix-core/src/policy.rs` with the `cedar-policy` Rust crate. Node-local policy bundles signed by org root. Entities: `Relix::Principal` (peer_id), `Relix::Resource` (capability method), `Relix::Action` (call). Audit records include matched rule + Cedar decision.

### Relix Web
Fork Open WebUI into `relix-web/` at repo root. `RELIX_MODE=true` env flag disables all direct provider connections. `backend/apps/relix/provider.py` POSTs to `http://localhost:8080/v1/chat/completions` (the web bridge OpenAI shim). Zero LLM calls from the backend in RELIX_MODE — hard invariant.

### Skill System
Skills are SKILL.md files stored in the memory node's SQLite DB (not filesystem). Capabilities: `skill.create`, `skill.get`, `skill.list`, `skill.patch`, `skill.delete`. Security scanner: 60+ threat patterns. Trust levels: builtin, user-created, agent-created, hub-installed. Progressive disclosure: list returns metadata only, get returns full content.

### Agent-Level Context Compression
`crates/relix-runtime/src/compressor.rs`. Fires at 50% context window. Three-pass tool result pruning (no LLM needed). LLM summarization with 12-section structured prompt. Critical invariant: last user message always in protected tail. Anti-thrashing: pause after 2 consecutive <10% saves.

### Live Telegram Channel
`crates/relix-telegram` scaffold is complete. Needs `reqwest`-backed `BotApi` impl and a bot token from @BotFather. Zero changes to memory/AI/tool nodes.

### Gemini Provider
`crates/relix-runtime/src/nodes/ai/provider/gemini.rs` is a 53-line stub. Needs live HTTP client following the same pattern as `anthropic.rs`.

---

## How to Write Prompts for Claude Code

Claude Code has full access to the Relix codebase. Good prompts:
- Name the exact crate: `crates/relix-core`, `crates/relix-runtime`, etc.
- Name the exact file: `crates/relix-runtime/src/nodes/router.rs`
- Reference an existing pattern: "follow the same pattern as the memory node in `crates/relix-runtime/src/nodes/memory/mod.rs`"
- State which architecture invariant must be preserved
- Say what test should pass when done

Bad prompts: "add X to Relix" (too vague, Claude will guess wrong paths)

Good prompt example:
> "In `crates/relix-core/src/policy.rs`, replace the current allowlist `PolicyEngine` with a Cedar-based engine using the `cedar-policy` Rust crate. The new `PolicyEngine::check(identity: &VerifiedIdentity, method: &str) -> PolicyDecision` must return `PolicyDecision { allowed: bool, matched_rule: Option<String> }`. All existing tests in that file must pass. Follow the same error type pattern as `crates/relix-core/src/audit.rs`."
