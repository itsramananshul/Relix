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

## Cron scheduler (`crates/relix-runtime/src/nodes/coordinator/cron/`)

Lives on the coordinator node. Six capabilities + a 30 s background
loop that fires due jobs through the same `task.create` path the
rest of the mesh uses. See [cron.md](cron.md) for the full design.

| Method | Status | Notes |
|---|---|---|
| `cron.create` | live | Arg: `name\|schedule\|flow_template\|prompt\|subject_id`. Schedule formats: duration (`30m`), 5-field cron (`0 9 * * 1`), RFC 3339 one-shot. Returns `<job_id>\n`. |
| `cron.list` | live | Arg: `<subject_id>` (empty = all). Tab-separated `<job_id>\t<name>\t<schedule>\t<next>\t<last>\t<enabled>\t<run_count>` rows newest-first, then `count=N\n`. |
| `cron.get` | live | Pipe-delim `key=value` body covering every column. Empty timestamps render as `-1`. |
| `cron.update` | live | Arg: `<job_id>\|<field>\|<value>` where field ∈ {`enabled`, `schedule`, `prompt`}. Updates `next_run_at` when `schedule` changes. |
| `cron.delete` | live | Permanent delete; returns `ok\n`. |
| `cron.trigger` | live | Manual fire — creates a coordinator task immediately and spawns ai.chat in the background. Returns the new `task_id\n`. Skipped (INVALID_ARGS) when the previous task is still `running`. |

## Memory node (`crates/relix-runtime/src/nodes/memory/`)

| Method | Status | Notes |
|---|---|---|
| `memory.write_turn` | live | Append one chat turn (session_id, role, body) |
| `memory.recent_for_session` | live | Read last N turns oldest-first (default 10) |
| `memory.search` | live | FTS5 search across all stored turns |
| `memory.agent_read` (W2-MEMORY-1) | live | Read persistent agent + user memory for a subject_id. Returns `agent_bytes=N\|user_bytes=M\n<bytes>`. See [agent-memory.md](agent-memory.md). |
| `memory.agent_write` (W2-MEMORY-1) | live | add/replace/remove/read one memory target. Arg: `subject_id\|target\|action\|data`. Targets: `agent` (cap 2200 chars) / `user` (cap 1375). Entries separated by `§`. |
| `memory.agent_curate` (W2-MEMORY-CURATOR) | live | Asks the AI peer to consolidate / drop stale entries for one subject's agent + user memory. Arg: `subject_id\|ai_peer_alias`. Returns pipe-delim summary with before/after counts. Existing memory is preserved on any AI failure. See [agent-memory.md](agent-memory.md#memory-curator). |
| `memory.curator_status` (W2-MEMORY-CURATOR) | live | Read-only view of the scheduler's live state: enabled flag, interval, last/next run timestamps, last run summary (agents_reviewed / agents_curated / total_chars_saved). Wire body is pipe-delim `key=value`, with `-1` sentinel for "no run yet". Bridge proxies as `GET /v1/memory/curator/status`. |

## Telegram channel node (`crates/relix-runtime/src/nodes/telegram/`)

| Method | Status | Notes |
|---|---|---|
| `telegram.status` | live | Read-only bot online state + identity. Arg: `""`. Returns `online=<bool>\|username=<str>\|first_name=<str>\|user_id=<i64>\|messages_seen=<u64>\|last_message_at=<i64>\n` (`-1` sentinel == "no message yet"). Bridge proxies as `GET /v1/telegram/status`. |
| `telegram.messages_recent` | live | Last N inbound messages from a bounded in-memory ring (capacity 200), newest-first. Arg: `<limit>` (defaults to 20). Returns tab-separated `ts\tfrom_user_id\tfrom_username\tchat_id\ttext_preview\n` rows. Preview is truncated to 100 chars; tabs / newlines replaced with spaces. Bridge proxies as `GET /v1/telegram/messages/recent?limit=N`. |

Outbound: the telegram node also dials `memory.write_turn`,
`memory.recent_for_session`, `memory.agent_read`,
`memory.agent_write`, `ai.chat`, `task.create`, `task.update`,
`task.event`, and `task.list` against its configured peers — these
aren't telegram-specific capabilities; they're the existing methods
the channel consumes as a normal mesh participant.

## AI node (`crates/relix-runtime/src/nodes/ai/`)

| Method | Status | Notes |
|---|---|---|
| `ai.chat` | live | Provider-agnostic; mock / openai-compat / anthropic / gemini-placeholder |
| Frozen-snapshot memory injection (W2-MEMORY-2) | live | When `[ai.memory_peer]` configured, ai.chat reads `memory.agent_read` once per call and prepends a labeled `--- AGENT MEMORY ---` / `--- USER MEMORY ---` block to `ChatInput.system_prompt`. Silent skip on any failure. |
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
| `tool.search_files` | live | Name / content substring or `glob` mode (PH-FS-PARITY3); linear walker |
| `tool.list_dir` (CW2) | live | Tab-separated rows + paginated |
| `tool.patch` | live | Unified diff apply |
| `tool.patch_preview` (PH-FS-PARITY1) | live | Read-only dry-run |
| `tool.binary_sniff` (PH-FS-PARITY2) | live | Classify text/binary by sniffing first 8 KiB |
| `tool.fs.audit_recent` (PH-FS-PARITY4) | live | Bounded ring of recent write/append/patch mutations |
| `tool.fuzzy_replace` (PH-FS-FUZZY) | live | Whitespace-tolerant text edit; refuses on 0 or >1 matches |
| `tool.fs.tree` (PH-FS-TREE) | live | Depth-capped recursive directory walk |
| `tool.fs.stat` (PH-FS-STAT) | live | Single-path metadata (kind/size/mtime/is_symlink/exists) |
| `tool.pdf` | live | base64-encoded PDF parse |
| `tool.text.chunk` (PH-PDF-CHUNK) | live | Split text into bounded chunks (paragraph > sentence > word > char) for retrieval / context-window prep |

### CLI surfaces

| Subcommand | Status | Notes |
|---|---|---|
| `relix-cli mcp servers --peer ...` (PH-MCP-CLI) | live | Lists MCP servers registered on a tool node (libp2p dial-and-call) |
| `relix-cli mcp tools --peer ... --server-id ...` (PH-MCP-CLI) | live | Lists declared tools for a specific MCP server |
| `relix-cli capability ls --risk <tier[+]>` (PH-CAP-RISK3) | live | Filter ls output by risk tier; `+` means at-or-above |
| `GET /v1/mcp/servers?peer=<alias>` (PH-BRIDGE-MCP) | live | Bridge HTTP proxy → tool.mcp.list_servers; returns structured JSON |
| `GET /v1/mcp/tools?peer=<alias>&server_id=<id>` (PH-BRIDGE-MCP) | live | Bridge HTTP proxy → tool.mcp.list_tools |
| `POST /v1/mcp/invoke` (PH-BRIDGE-MCP-INVOKE) | live (surface) | Bridge HTTP proxy → tool.mcp.invoke; returns 502 RuntimeNotConnected until D-009 unblocks |
| `relix-cli terminal sessions --peer ...` (PH-TERM-CLI) | live | Lists live in-flight terminal sessions |
| `relix-cli terminal audit --peer ... [--max N]` (PH-TERM-CLI) | live | Snapshots the completion ring; renders status (ok/timed_out/cancelled) |
| `relix-cli terminal cancel --peer ... --session-id ...` (PH-TERM-CLI) | live | Triggers cooperative cancel for a live session |

### Web

| Method | Status | Notes |
|---|---|---|
| `tool.web_fetch` | live | SSRF + DNS pin + per-hop redirect re-check |
| `tool.web.post` (PH-WEB-POST) | live | HTTP POST with body + raw cookie header; surfaces Set-Cookie verbatim |
| `tool.web_extract` | live | Hand-rolled HTML state machine — modes: text/title/links/meta/markdown (PH-WEB-MARKDOWN)/all |
| `tool.web_get` (CW3) | live | Fetch + extract in one call |
| `tool.web_search` (CW3) | live | DuckDuckGo HTML scrape |
| `tool.web.robots_check` (PH-WEB-ROBOTS) | live | robots.txt sniff + RFC 9309 longest-prefix-match-wins; defaults to allow on missing |
| `tool.web.blocklist_summary` (PH-DASH-BLOCKLIST) | live | Read-only snapshot of `[tool] blocked_hosts` (PH-WEB-BLOCKLIST); used by `#/fsaudit` dashboard card + `relix-cli web blocklist` |

### Terminal

| Method | Status | Notes |
|---|---|---|
| `tool.terminal.run` (CW1) | live | Sandboxed shell; operator allowlist required |
| `tool.terminal.spawn` (PH-TERM-SPAWN) | live | Fire-and-forget variant of run; returns session_id immediately |
| `tool.terminal.shell.open` (PH-TERM-SHELL) | live | Open a persistent shell session (separate `allowed_shells` allowlist) |
| `tool.terminal.shell.input` (PH-TERM-SHELL) | live | Write bytes (UTF-8 or base64) to a shell session's stdin |
| `tool.terminal.shell.close` (PH-TERM-SHELL) | live | Close shell stdin (signal EOF); does not kill the child |
| `tool.terminal.shell.control` (PH-TERM-CONTROL) | live | Write named control char (etx/eot/tab/enter/esc/...) to shell stdin |
| `tool.terminal.sessions` (PH-TERM-SESSIONS) | live | Live in-flight run registry snapshot |
| `tool.terminal.audit_recent` (PH-TERM-AUDIT) | live | Bounded ring of completed runs (success + timed-out + cancelled) |
| `tool.terminal.cancel` (PH-TERM-CANCEL) | live | Cooperatively terminate a live `tool.terminal.run` session by id |
| `tool.terminal.tail` (PH-TERM-STREAM1) | live | Polling-cursor stream tail of a live run's stdout / stderr buffer |

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

### Router node (PH-ROUTER-NODE)

A new role for the controller binary — same `relix-controller`
binary, different `[controller] role = "router"`. Acts as the
mesh's observability + health control plane. Receives
heartbeats from every controller and answers operator-facing
`router.*` queries. Never makes LLM calls, never holds provider
keys, runs every RPC through the existing identity → policy →
handler → audit pipeline.

| Method | Status | Notes |
|---|---|---|
| `router.heartbeat` | live | Controller-only push; registers/updates peer + caps + groups |
| `router.network_summary` | live | Operator-facing mesh overview (peers, active sessions, uptime); `org_filter` substring |
| `router.session_list` | live | Operator-facing session browser; `status_filter` + `limit` + `offset` pagination |
| `router.log` | live | Controller-only push; bounded 10k-line in-memory ring |

Background loops (router role only): stale-peer reaper every
30s (flips `healthy=false` after 90s of no heartbeat); session
reaper every 300s (drops `completed`/`failed` sessions past
`session_ttl_secs`, default 1800).

Controllers in `controller` role with a non-empty
`router_peer_id` spawn a 60-second heartbeat sender (1.5s
warmup, then every 60s). Bundle loaded from
`<key_path>.bundle`; missing bundle disables heartbeats with
a single WARN line (controller still boots).

See `configs/router-node.toml` and `configs/policies/router.toml`.

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
| `POST /v1/providers/route_test` (PH-ROUTER-PREVIEW) | Preview HealthAwareRouter's pick for a candidate list against current cached health |
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
| `relix-cli ops route-test` (PH-ROUTER-PREVIEW-CLI) | Preview HealthAwareRouter pick over current cached health |
| `relix-cli router status` (PH-ROUTER-NODE) | Router mesh overview via `router.network_summary` |
| `relix-cli router peers` (PH-ROUTER-NODE) | Per-peer table from `router.network_summary` |
| `relix-cli router sessions` (PH-ROUTER-NODE) | Session browser via `router.session_list` (--status/--limit/--offset) |
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
