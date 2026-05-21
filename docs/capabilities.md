# Capabilities catalog

This is the operator's index of every Relix capability that ships
with the runtime. Each row links to the source file + the
per-capability doc when one exists.

For the canonical wire-format reference, read the source file:
each capability ships its descriptor (`pub fn descriptor_…() ->
CapabilityDescriptor`) right above its handler, and the docstring
on the surrounding module documents the wire format precisely.

## What "shipped" means

| Status | What you can do today |
|---|---|
| `live` | Full implementation. Operators can invoke and get real results. |
| `scaffold` | Capability descriptor + dispatch + error envelope ship. Live execution returns a typed `BackendNotConnected` / `RuntimeNotConnected` error explaining the gap. Operators see the surface; future milestones wire the backend. |

Every `scaffold` entry has an explicit doc explaining why it ships
before the backend (visibility + stable contract + honesty).

## Coordinator (`crates/relix-runtime/src/nodes/coordinator/`)

| Method | Status | Notes |
|---|---|---|
| `task.create` / `task.update` / `task.get` / `task.list` / `task.cursor` | live | Core lifecycle |
| `task.attempts` / `task.events` / `task.recent_events` | live | Read-side projections |
| `task.lineage` / `task.subtree_metrics` (M75) | live | Graph walks |
| `task.spawned_child` / `task.delegated_to` / `task.awaiting` (M72) | live | Cross-task edge producers |
| `task.pause_requested` / `task.resume_requested` (M65/M70) | live | Cooperative interruption (intent) |
| `task.freeze_requested` / `task.unfreeze_requested` (M71) | live | Workflow-level interruption |
| `task.pause_observed` / `task.resume_observed` / `task.freeze_propagated` (M70) | live | Cooperative interruption (ack) |
| `task.retry` (M76 + H4) | live | Anti-thrash auto-mark after 3 same-class fails |
| `task.note` (M60) | live | Operator notes (H8 redaction at write boundary) |
| `task.mark_investigation` (M62) | live | Investigation flag |
| `task.stuck` (H6) | live | Running-without-deadline projection |
| `task.todo_set` / `task.todo_list` / `task.todo_update` (PH-WAVE2D) | live | Per-task ordered todo list |
| `task.transition_check` (M74) | live | State-machine matrix validator |
| `task.export` / `task.compact_events` | live | Chronicle retention surfaces |
| `task.thrash_detected` (H4) | live | Auto-emitted when consecutive_same_class_count crosses threshold |
| `task.terminal_summary` (H5 + H14) | live | Auto-emitted on every terminal transition |
| `task.attempt_orphan_closed` (H7) | live | Recovery-scan cleanup of orphaned attempts |

## AI node (`crates/relix-runtime/src/nodes/ai/`)

| Method | Status | Notes |
|---|---|---|
| `ai.chat` | live | Provider-agnostic; mock / openai-compat / anthropic / gemini-placeholder |
| Anthropic prompt caching (PH-WAVE2E) | live | `cache_control: ephemeral` on system block |
| Anthropic extended thinking (PH-WAVE2F) | live | Opt-in `thinking_budget_tokens` on ChatInput |
| `FailoverReason` classifier (H1) | live | 12 categories + retry hint |
| `NoopRouter` / `ProviderRouter` trait (PH-ROUTER1) | live | Foundation for future smart router; preserves single-provider behavior |
| `HealthAwareRouter` + `ProviderHealth` (PH-ROUTER2) | live | Health-aware router that filters cooldown/quarantined providers and ranks by success_ratio; falls back to best ratio when all unhealthy |

## Tool node (`crates/relix-runtime/src/nodes/tool/`)

### Filesystem (jailed)

| Method | Status | Notes |
|---|---|---|
| `tool.read_file` | live | UTF-8 only; configurable byte cap |
| `tool.write_file` | live | Atomic; modes overwrite / create_new |
| `tool.append_file` (PH-FS-PARITY1) | live | Strictly additive; refuses to create |
| `tool.search_files` | live | Name or substring-content; linear walker |
| `tool.list_dir` (CW2) | live | Tab-separated rows + paginated |
| `tool.patch` | live | Unified diff apply |
| `tool.patch_preview` (PH-FS-PARITY1) | live | Read-only dry-run |
| `tool.binary_sniff` (PH-FS-PARITY2) | live | Classify text/binary by sniffing first 8 KiB |
| `tool.pdf` | live | base64-encoded PDF parse |

### Web

| Method | Status | Notes |
|---|---|---|
| `tool.web_fetch` | live | SSRF + DNS pin + per-hop redirect re-check |
| `tool.web_extract` | live | Hand-rolled HTML state machine |
| `tool.web_get` (CW3) | live | Fetch + extract in one call |
| `tool.web_search` (CW3) | live | DuckDuckGo HTML scrape |
| `tool.web.robots_check` (PH-WEB-ROBOTS) | live | robots.txt sniff + RFC 9309 longest-prefix-match-wins; defaults to allow on missing |

### Terminal

| Method | Status | Notes |
|---|---|---|
| `tool.terminal.run` (CW1) | live | Sandboxed shell; operator allowlist required |

### Browser (CW4 honest scaffold)

| Method | Status | Notes |
|---|---|---|
| `tool.browser.open_session` | live | Allocates session id; tracks in-memory |
| `tool.browser.close_session` | live | Idempotent |
| `tool.browser.list_sessions` | live | Status reads `unconnected` |
| `tool.browser.navigate` | scaffold | Returns `BackendNotConnected` — wired surface, no Playwright yet |
| `tool.browser.get_text` | scaffold | Same |
| `tool.browser.screenshot` | scaffold | Same |

See `docs/browser-tool.md`.

### MCP (CW5 honest scaffold)

| Method | Status | Notes |
|---|---|---|
| `tool.mcp.list_servers` | live | Returns operator-declared servers with `status="configured"` |
| `tool.mcp.list_tools` | live | Returns operator-declared tool list (never fabricated) |
| `tool.mcp.invoke` | scaffold | Returns `RuntimeNotConnected` — registry + descriptor live, client pending |

See `docs/mcp-tool.md`.

## Bridge surface (`crates/relix-web-bridge/src/`)

The bridge translates HTTP → coord capabilities. Notable
operator-facing endpoints:

| Endpoint | Notes |
|---|---|
| `GET /v1/tasks/*` | Read-side projections of the coord ledger |
| `GET /v1/tasks/:id/todos` / `PUT` / `PATCH` (PH-DASH2) | Per-task todo CRUD |
| `GET /v1/tasks/stuck` (H6) | Stuck-running projection |
| `GET /v1/tasks/events/recent` / `/stream` (M67/M73) | Cross-task firehose + SSE |
| `GET /v1/providers/health` (PH-WAVE2K) | Consolidated AI-stack snapshot |
| `GET /v1/config/providers` | Per-provider redacted status |
| Route-latency tracing middleware (H15) | Structured log field per request |
| Operator intervention audit (M57 + H9) | All mutating routes recorded; H9 redacts |

## Operator UI (`crates/relix-web-bridge/src/dashboard.html`)

| Page | Notes |
|---|---|
| `#/overview` | Mesh + runtime KPIs, ops-health badges (H11), redaction counter (PH-WAVE2C), event-type histogram (H13), stuck banner (H6), firehose with filter (H12) + row category accents (PH-WAVE2H) + click-to-expand payload (PH-DASH1) |
| `#/tasks` | Ledger + per-task drill-in (timeline, retry chain, exec graph, lineage, todos widget) |
| `#/topology` | Mesh + cross-task edges + lifecycle events |
| `#/capabilities` (PH-DASH3) | Capability explorer with category chips + substring filter |
| `#/providers` | AI provider CRUD + per-card routing-trace badge (M77) + failover-reason badge (H1) + rate-limit time-decay badge (PH-WAVE2G) + auto-cooldown banner (PH-WAVE2J) |
| `#/telegram` | Telegram channel config |
| `#/config` | Bridge config inspection |

## CLI (`crates/relix-cli/`)

| Command | Notes |
|---|---|
| `relix-cli identity` | Mint / inspect identity bundles |
| `relix-cli task` | Operate the coord ledger |
| `relix-cli capability ls` | Per-peer manifest dump |
| `relix-cli topology show / health` | Topology + bridge health |
| `relix-cli flow-run` | SOL flow execution |
| `relix-cli ops providers-health` (PH-WAVE2L) | Consolidated AI-stack snapshot |
| `relix-cli ops capabilities` (PH-DASH3-CLI) | Mesh-wide capability list |
| `relix-cli ops stuck` (PH-OPS-STUCK) | H6 stuck-running projection |
| `relix-cli ops events` (PH-OPS-EVENTS) | H2 firehose snapshot for terminal operators |
| `relix-cli ping` | Direct libp2p ping to a peer |

## Per-feature docs

When a capability has subsystem-specific contracts, see the
dedicated doc:

- `docs/browser-tool.md` — CW4 honesty contract + Playwright roadmap
- `docs/mcp-tool.md` — CW5 honesty contract + live-client roadmap
- `docs/tool-node-security.md` — SSRF, terminal allowlist, jail discipline
- `docs/chronicle-retention.md` — Chronicle compaction design
- `docs/event-contract.md` — Chronicle event_type vocabulary
- `docs/event-vocabulary.md` — H2 one-line summary projection rules
- `docs/security.md` — Top-level admission pipeline
- `docs/bridge-invariants.md` — What the bridge MAY / MUST NOT do
- `docs/operator-guide.md` — Logs + common failures + CLI surface

## Internal-only

These are operator-internal docs, not part of the public catalog:

- `docs/internal/continuation-state.md` — Autonomous-run handoff
- `docs/internal/decisions-pending.md` — Open operator decisions
- `docs/internal/hermes-capability-map.md` — Hermes parity inventory
