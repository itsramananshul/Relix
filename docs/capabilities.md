# Capabilities catalog

Version: **0.4.1**

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

---

## Built-in (every node, `crates/relix-runtime/src/controller_runtime.rs`)

Every controller — regardless of `node_type` — registers these capabilities.

| Method | Status | Purpose |
|---|---|---|
| `node.health` | live | Runtime identity + version ping (`name`, `type`, `status`, `runtime=0.4.1`). |
| `node.manifest` | live | Ed25519-signed `SignedManifest` (CBOR) listing all registered capabilities for this node. |
| `node.dispatch.stats` | live | Per-capability invocation counters, latency ring, and error counts (tab-delimited). |
| `node.policy.simulate` | live | Dry-run a policy decision: arg `<method>\|<groups_csv>` → `decision/matched_rule/reason`. |
| `node.policy.recent_denials` | live | Bounded ring of recent `POLICY_DENIED` events (default 100, cap 500, ring depth 256). |
| `node.policy.tenant_list` | live | List all per-tenant policy ids configured on this node. |
| `node.policy.tenant_get` | live | Retrieve raw TOML for a tenant policy by id. |
| `node.audit.tenant_list` | live | List tenant ids in the SQLite audit partition mirror (requires `[audit] partition_by_tenant = true`). |
| `node.audit.tenant_recent` | live | Per-tenant audit rows from the partition mirror: arg `<tenant_id>\|<limit>` → JSON `{tenant_id, count, rows}`. |

---

## Coordinator (`crates/relix-runtime/src/nodes/coordinator/`)

### Task ledger

| Method | Status | Notes |
|---|---|---|
| `task.create` | live | Core lifecycle: `title\|flow_template\|params_json\|owner_subject_id\|retry_policy\|max_retries\|max_runtime_secs` → 32-hex `task_id`. |
| `task.update` | live | Status + result fields: `task_id\|status\|result\|flow_id\|flow_log_path\|error_kind\|error_cause\|failure_class\|trace_id`. |
| `task.event` | live | Append a chronicle event: `task_id\|event_type\|payload` → `event_id`. |
| `task.get` | live | Full `TaskView` as `key=value` + `events:` JSON array. |
| `task.list` | live | Paginated summary rows; accepts `limit\|offset\|status`. Default limit 50. |
| `task.list_cursor` | live | Cursor-based stable pagination (`<updated_at>:<task_id>`); returns `next_cursor=` trailer. |
| `task.count` | live | `count=N` for optional status filter. |
| `task.events` | live | Per-task chronicle events: `task_id\|after_id\|limit\|type\|order` → JSON event objects. |
| `task.attempts` | live | Per-attempt timeline for a task. |
| `task.recent_events` | live | Cross-task event firehose: `since_event_id\|limit\|event_type` → JSON array with `task_id`. |
| `task.lineage` | live | BFS lineage walk from a root task: → JSON `{root_task_id, tasks, edges, cross_task_edge_count, max_depth_walked}`. |
| `task.subtree_metrics` | live | Aggregate metrics over a task subtree: → JSON metrics object. |
| `task.spawned_child` | live | **Chronicle edge producer** — records a `spawned` cross-task edge. |
| `task.delegated_to` | live | **Chronicle edge producer** — records a `delegated_to` cross-task edge. |
| `task.awaiting` | live | **Chronicle edge producer** — records an `awaited` cross-task edge. |
| `task.record_spawned` | live | Explicit edge producer: `parent\|child\|branch_id\|context_id\|producer` → `edge_id event_id`. |
| `task.record_delegated` | live | Explicit edge producer: `parent\|child\|reason\|producer` → `edge_id event_id`. |
| `task.record_awaited` | live | Explicit edge producer: `parent\|awaited\|reason\|producer` → `edge_id event_id`. |
| `task.edges` | live | Edge list for a task. |
| `task.recent_edges` | live | Newest-first cross-task edges since a cursor. |
| `task.pause` | live | Cooperative interruption (intent): flips to `paused`, bumps `pause_generation`. |
| `task.resume` | live | Cooperative interruption: flips to `pending`, bumps `pause_generation`. |
| `task.freeze` | live | Workflow-level freeze: flips to `frozen`, bumps `freeze_generation`. |
| `task.unfreeze` | live | Unfreeze: flips to `pending`, clears `frozen_at`. |
| `task.interruption_check` | live | Reads current `status`, `pause_generation`, `freeze_generation` for polling runtimes. |
| `task.observe_interruption` | live | Runtime ack of a cooperative interruption (emits `pause_observed` / `freeze_propagated` events). |
| `task.pause_observed` | live | **Chronicle event type** emitted by `task.observe_interruption` for pause acks. |
| `task.resume_observed` | live | **Chronicle event type** emitted by `task.observe_interruption` for resume acks. |
| `task.freeze_propagated` | live | **Chronicle event type** emitted by `task.observe_interruption` for freeze acks. |
| `task.retry` | live | Request retry from `failed`/`interrupted`; enforces `retry_policy` + anti-thrash detection (H4). |
| `task.replay` | live | Clone a task to a new id; records `retried_from` cross-task edge. |
| `task.note` | live | Append an operator note (H8 redaction at write boundary). |
| `task.mark_investigation` | live | Set/clear investigation marker with reason. |
| `task.stuck` | live | Running-without-deadline projection: arg `threshold_secs` → JSON array of stuck-task rows. |
| `task.todo_set` | live | Replace the per-task todo list with an ordered text list. |
| `task.todo_list` | live | Read the current ordered todo list. |
| `task.todo_update` | live | Update one todo item status (`open`/`done`). |
| `task.transition_check` | live | State-machine matrix validator: `task_id\|target_status` → `allowed=true/false`. |
| `task.export` | live | Full archival export as JSON (`schema_version`, `exported_at`, `task`, `attempts`). |
| `task.compact_events` | live | Chronicle retention dry-run analysis (destructive mode not implemented). |
| `task.recover` | live | Startup recovery scan: promotes stale `running` tasks to `interrupted`. |
| `task.session_export` | live | Chat-turn chronicle query for a session: → JSON array of `ChatTurn`. |
| `task.session_search` | live | Full-text search over chat turns: `subject_id\|query\|limit` → JSON array of hits. |
| `task.thrash_detected` | live | **Auto-emitted** chronicle event when `consecutive_same_class_count` ≥ 3 (H4). |
| `task.terminal_summary` | live | **Auto-emitted** chronicle event on every terminal status transition (H5/H14). |
| `task.attempt_orphan_closed` | live | **Auto-emitted** chronicle event when orphaned open attempts are closed (H7). |

### Agent employee model (`nodes/coordinator/agent/`)

Categorical permissions narrow what policy allows — they never widen what policy denies. See
[agent-permissions.md](agent-permissions.md) for the full design.

| Method | Status | Notes |
|---|---|---|
| `agent.create` | live | Create agent profile: `name\|role\|title\|department\|team\|created_by\|subject_id\|risk_ceiling` → `<agent_id>`. |
| `agent.get` | live | Read profile as pipe-delimited `key=value` body. |
| `agent.list` | live | Tab-separated rows + `count=N`. |
| `agent.update` | live | Update one field: `agent_id\|field\|value`. Settable fields: `status`, `role`, `title`, `department`, `team`, `surface_allowlist`, `risk_ceiling`, `allow_categories`, `deny_categories`, `allow_sensitivity_tags`, `deny_sensitivity_tags`, `approval_required_categories`, `approval_timeout_secs`. |
| `agent.delete` | live | Soft delete — flips `status = disabled`. |
| `agent.effective_capabilities` | live | Intersect an agent's permissions with a peer's manifest: one method per line + `count=N`. |
| `agent.standing_approval.create` | live | Grant standing approval: `agent_id\|category\|expires_at\|granted_by\|note\|path_glob?` → `<standing_id>`. |
| `agent.standing_approval.list` | live | Per-agent standing approvals. |
| `agent.standing_approval.revoke` | live | Delete by `standing_id`. |
| `coord.approval.pending` | live | Newest-first pending approvals. |
| `coord.approval.decide` | live | `approval_id\|approved\|decided_by\|note` → `ok\|<token>` (mints one-shot Ed25519 token) or `ok`. |

Two error kinds:
- `APPROVAL_REQUIRED` (19) — gate matches an `approval_required_categories` entry without an active standing approval.
- `APPROVAL_TOKEN_INVALID` (20) — token is unknown, expired, consumed, or applies to a different method.

### Agent-to-agent messaging (`nodes/coordinator/messaging/`)

Direct point-to-point mail-drop; stored on the coordinator alongside the task ledger. A 5-minute background loop sweeps past-TTL messages. See [messaging.md](messaging.md).

| Method | Status | Notes |
|---|---|---|
| `msg.send` | live | `from\|to\|subject\|body\|thread_id\|reply_to\|ttl_secs\|origin_surface`. Empty `thread_id` starts a new thread. Default TTL 86400 s. |
| `msg.inbox` | live | Cursor-paginated newest-first inbox: `subject_id\|limit\|include_read\|since_message_id`. |
| `msg.read` | live | Mark message read (idempotent): `message_id\|reader_subject_id`. |
| `msg.thread` | live | Oldest-first thread view: `thread_id\|subject_id`. Caller must be a participant. |
| `msg.delete` | live | Soft delete (flips to `expired`): `message_id\|subject_id`. Sender or recipient only. |

### Delegation (`nodes/coordinator/delegate/`)

A 5-second background executor dispatches `ai.chat` for pending child tasks. See [delegation.md](delegation.md).

| Method | Status | Notes |
|---|---|---|
| `delegate.spawn` | live | `parent_task_id\|goal\|context\|target_subject_id\|depth` → `<child_task_id>`. Depth cap enforced + ancestor-chain walk. |
| `delegate.result` | live | `child_task_id` → `status\|preview\|completed_at` (`-1` if not terminal; preview ≤ 500 chars). |
| `delegate.cancel` | live | `<child_task_id>\|<reason>` → `ok`. Refuses when already terminal. |
| `delegate.list` | live | `parent_task_id` → tab-separated `child\tgoal\tstatus\tcreated_at` rows + `count=N`. |

### Cron scheduler (`nodes/coordinator/cron/`)

A 30-second background loop fires due jobs through `task.create`. See [scheduler.md](scheduler.md).

| Method | Status | Notes |
|---|---|---|
| `cron.create` | live | `name\|schedule\|flow_template\|prompt\|subject_id` → `<job_id>`. Schedule: 5-field cron, duration (`30m`), or RFC 3339 one-shot. |
| `cron.list` | live | Summary rows (newest-first) + `count=N`; optional `subject_id` filter. |
| `cron.get` | live | Full job details as pipe-delimited `key=value`. |
| `cron.update` | live | `<job_id>\|<field>\|<value>` where field ∈ `{enabled, schedule, prompt}`. |
| `cron.delete` | live | Permanent delete → `ok`. |
| `cron.trigger` | live | Manual fire → new `task_id`; skips (`INVALID_ARGS`) if previous task still `running`. |

### Channel routing

| Method | Status | Notes |
|---|---|---|
| `routing.resolve` | live | JSON `InboundMessage` → JSON `{decision, rules_evaluated}`. Used by channel nodes to route inbound messages. |
| `routing.list` | live | JSON array of all configured `RoutingRule`s. |

---

## Memory node (`crates/relix-runtime/src/nodes/memory/`)

The memory node exposes two coexisting stores: the Hermes-style store (SQLite chat turns +
per-subject agent/user blobs + embedding store) and the four-layer store (bi-temporal
`memory_records` table with Raw/Semantic/Observation/Model layers, Qdrant vector mirroring,
and a promoter pipeline). Capabilities marked **[layered]** require `[memory.qdrant]` to be
configured.

### Core (Hermes-style store — always registered)

| Method | Status | Notes |
|---|---|---|
| `memory.write_turn` | live | Append one chat turn: `session_id\|role\|body` → `ok`. Also inserts a Layer 1 Raw record when layered store is configured. |
| `memory.recent_for_session` | live | Last N turns oldest-first: `session_id` or `session_id\|N` (default 10). |
| `memory.search_turns` | live | FTS5 full-text search over turns: `query` or `query\|N` → tab-separated rows. |
| `memory.agent_read` | live | Read persistent agent + user memory blobs: `subject_id` → `agent_bytes=N\|user_bytes=M` + raw bytes. |
| `memory.agent_write` | live | Write/read one memory target: `subject_id\|target\|action\|data`. Targets: `agent` (cap 2200 chars) / `user` (cap 1375 chars). |
| `memory.agent_curate` | live | Ask AI peer to consolidate/drop stale memory entries: `subject_id\|ai_peer_alias` → pipe-delimited result. |
| `memory.curator_status` | live | Read-only scheduler state: enabled, interval, last/next run, `agents_reviewed/curated/chars_saved` (pipe-delimited `key=value`). |
| `memory.embed` | live | Embed and store a text chunk: `subject_id\|target\|text` → `embedding_id=<id>`. |
| `memory.search` | live | Per-subject vector search (Hermes embedding store): `subject_id\|target\|query[|limit][|embedding=<b64>]` → `id\tscore\tchunk` rows. |
| `memory.embed_all` | live | Re-embed all chunks for a subject: `subject_id` → `ok\|chunks_embedded=N`. |
| `memory.session_search` | live | Proxy to coordinator `task.session_search`: `subject_id\|query[|limit]` → JSON session hit array. |
| `memory.pii_scan` | live | Scan arbitrary text for PII: JSON `{"text":"…"}` → JSON `{"spans":[…],"count":N}`. Always registered regardless of `[memory.pii]` setting. |
| `memory.anonymize_preview` | live | Preview anonymization without enabling production scrubbing: JSON `{"text":"…","strategy":"redact\|pseudonymize\|allow"}` → JSON `{"anonymized":"…","spans":[…]}`. |
| `memory.bulk_anonymize` | live | Idempotent in-place PII anonymization of all stored records. Only callable when `[memory.pii] enabled = true`. |

### Four-layer store (registered only when `[memory.qdrant]` configured)

| Method | Status | Notes |
|---|---|---|
| `memory.records_search` | live | **[layered]** Qdrant-backed semantic search over `memory_records`: `query` or `query\|N` → `id\tlayer\tsource\tscore\ttext` rows; SQLite `LIKE` fallback when Qdrant unavailable. Score threshold default 0.75. |
| `memory.dialectic` | live | **[layered]** LLM-powered synthesis combining Layer 4 model + top-5 Layer 3 observations: JSON `{observer_id, subject_id, question}` → JSON `{answer, confidence, sources_used, model_used, fallback_reason?}`. |
| `memory.ingest_document` | live | **[layered]** Ingest a document (text/markdown/code/pdf/url) into Layer 2 Semantic. JSON args + response. |
| `memory.ingest_image` | live | **[layered]** Ingest an image (≤ 25 MiB) into Layer 2 Semantic. JSON args + response. |
| `memory.context_flush` | live | **[layered]** Flush unflushed turns to Layer 2 Semantic: JSON `{session_id, agent_name, keep_recent_n}`. |
| `memory.quarantine_list` | live | **[layered]** List anomaly-quarantined records awaiting operator review. |
| `memory.quarantine_approve` | live | **[layered]** Approve a quarantined record (re-inserts into `memory_records`). |
| `memory.quarantine_reject` | live | **[layered]** Permanently delete a quarantined record. |
| `memory.edit_record` | live | **[layered]** Overwrite the text of a record (clears embedding for re-embedding). |
| `memory.freeze_record` | live | **[layered]** Freeze a record so it survives curator consolidation and archival. |
| `memory.unfreeze_record` | live | **[layered]** Unfreeze a frozen record. |
| `memory.bulk_export` | live | **[layered]** Export records by source and optional layer. |
| `memory.request_model_refresh` | live | **[layered]** Trigger Layer 4 model regeneration for a source. |

---

## AI node (`crates/relix-runtime/src/nodes/ai/`)

| Method | Status | Notes |
|---|---|---|
| `ai.chat` | live | Provider-agnostic chat; runs full pipeline (guardrail → memory/RAG → soul → skills → belief → routing → provider → SC → judge → planner/tool → policy → provenance). Providers: `mock`, `openai`, `openrouter`, `xai`, `local`, `anthropic`, `gemini`. |
| `ai.chat.stream` | live | Streaming variant; same preflight as `ai.chat`; skips planner, tool dispatch, approval verdict, judge, belief update. Uses native SSE streaming from the provider. |
| `ai.embed` | live | Batch embeddings: `model\|text1§text2§…` → `model\|base64(LE f32)\|…\n`. |
| `ai.perception_extract` | live | Two-stage isolation for screen/tool output: JSON `{content, instructions, max_output_chars}` → JSON `{extracted, model, isolated}`. Disabled config returns `isolated:false`. |
| `routing.explain` | live | Dry-run complexity classifier + tier resolver (always registered regardless of `[ai.routing] enabled`): JSON `{message, session_turns}` → JSON `{score, decision, routing_enabled}`. |
| `belief.get` | live | Read current belief set for `(subject_id, session_id)`: JSON → JSON belief array. |
| `belief.reset` | live | Clear belief state for a `(subject_id, session_id)` pair. |
| `judge.recent_verdicts` | live | Ring of recent judge verdicts: JSON `{limit}` → JSON `{verdicts:[…]}`. |
| `judge.stats` | live | Aggregate judge statistics: proceed/modify/block/timeout counts + per-agent breakdown. |
| `reasoning.status` | live | Snapshot of all §7.29 component state (tier routing, SC, belief, judge). Always registered. |

---

## Tool node (`crates/relix-runtime/src/nodes/tool/`)

### Filesystem (jailed, requires `[tool.fs]`)

| Method | Status | Risk | Notes |
|---|---|---|---|
| `tool.read_file` | live | Low | UTF-8 only; configurable byte cap (default 10 MiB). Rejects non-regular files. |
| `tool.write_file` | live | Medium | Atomic (tempfile + rename); modes `overwrite` / `create_new`. |
| `tool.append_file` | live | Medium | Strictly additive; refuses to create; cap against appended length. |
| `tool.search_files` | live | Low | Name / content substring / `glob` mode; linear walker. |
| `tool.list_dir` | live | Low | Tab-separated `kind\tname\tsize\tmtime` rows; paginated. |
| `tool.patch` | live | Medium | Unified diff apply; atomic write. |
| `tool.patch_preview` | live | Low | Read-only dry-run of a unified diff. |
| `tool.binary_sniff` | live | Low | Classify text/binary by sniffing first 8 KiB. |
| `tool.fs.audit_recent` | live | Low | Bounded ring (256) of recent write/append/patch/fuzzy_replace mutations. |
| `tool.fuzzy_replace` | live | Medium | Whitespace-tolerant text edit; refuses on 0 or >1 matches. |
| `tool.fs.tree` | live | Low | Depth-capped recursive directory walk (default depth 5). |
| `tool.fs.stat` | live | Low | Single-path metadata: `kind/size/mtime/is_symlink/exists`. |

### Document parsing (always registered; `[tool.parse_document]` controls cloud tiers)

| Method | Status | Notes |
|---|---|---|
| `tool.pdf` | live | Requires `[tool.pdf]`. Base64-encoded PDF parse via lopdf (no OCR). Modes: `text`, `pages`, `meta`, `all`. |
| `tool.parse_document` | live | Tiered pipeline: LlamaParse → Jina → Firecrawl → local. JSON args `{kind, payload, source?}`. |
| `tool.web_read` | live | URL-only pipeline (Jina → Firecrawl → local fetch+extract). |
| `tool.text.chunk` | live | Split text into bounded chunks (paragraph > sentence > word > char). JSON `{text, chunk_size, chunk_overlap?}`. |

### Web (always registered)

| Method | Status | Risk | Notes |
|---|---|---|---|
| `tool.web_fetch` | live | Medium | SSRF + DNS pin + per-hop redirect re-check; body cap default 256 KiB. |
| `tool.web.post` | live | Medium | HTTP POST with body + raw cookie header; surfaces `Set-Cookie` verbatim. |
| `tool.web_extract` | live | Low | Hand-rolled HTML state machine; modes: `text/title/links/meta/markdown/all`. |
| `tool.web_get` | live | Medium | Fetch + extract in one call; supports `raw` mode. |
| `tool.web_search` | live | Low | DuckDuckGo HTML scrape; max 20 results. |
| `tool.web.robots_check` | live | Low | robots.txt sniff + RFC 9309 longest-prefix-match-wins; defaults to allow on missing. |
| `tool.web.blocklist_summary` | live | Low | Read-only snapshot of `[tool] blocked_hosts`; no network. |

### Terminal (requires `[tool.terminal]`)

| Method | Status | Risk | Notes |
|---|---|---|---|
| `tool.terminal.run` | live | High | Sandboxed shell (operator `allowed_commands` required); waits for completion. |
| `tool.terminal.spawn` | live | High | Fire-and-forget variant; returns `session_id` immediately. |
| `tool.terminal.shell.open` | live | High | Open a persistent shell session (separate `allowed_shells` allowlist). |
| `tool.terminal.shell.input` | live | High | Write bytes (UTF-8 or base64) to shell stdin. |
| `tool.terminal.shell.close` | live | Low | Close shell stdin (EOF); does not kill the child process. |
| `tool.terminal.shell.control` | live | High | Write named control char (`etx/eot/tab/enter/esc/…`) to shell stdin. |
| `tool.terminal.sessions` | live | Low | Live in-flight run registry snapshot. |
| `tool.terminal.audit_recent` | live | Low | Bounded ring (256) of completed runs (success + timed-out + cancelled). |
| `tool.terminal.cancel` | live | Low | Cooperatively terminate a live run by session id. |
| `tool.terminal.tail` | live | Low | Polling-cursor stream tail of a live run's stdout/stderr buffer. |

### Browser (requires `[tool.browser]`)

All browser capabilities that perform real navigation require a compiled + running backend
(`headless_chrome`, `playwright`, or `webdriver`). The `none` backend (default) allows
session management but returns `BackendNotConnected` on any navigation call.

| Method | Status | Notes |
|---|---|---|
| `tool.browser.open_session` | live | Allocates session id; tracks in-memory. |
| `tool.browser.close_session` | live | Idempotent close. |
| `tool.browser.list_sessions` | live | Status reads `"unconnected"` on `NoneBackend`. |
| `tool.browser.navigate` | scaffold | Returns `BackendNotConnected` on `NoneBackend`. |
| `tool.browser.get_text` | scaffold | Returns `BackendNotConnected` on `NoneBackend`. |
| `tool.browser.screenshot` | scaffold | Returns `BackendNotConnected` on `NoneBackend`. |
| `tool.browser.click` | scaffold | Returns `BackendNotConnected` on `NoneBackend`. |
| `tool.browser.type_text` | scaffold | Returns `BackendNotConnected` on `NoneBackend`. |
| `tool.browser.wait_for_selector` | scaffold | Returns `BackendNotConnected` on `NoneBackend`. |
| `tool.browser.capture_read` | live | Read a failure-screenshot PNG by basename from `screenshot_on_failure_dir`. |

See `docs/browser-tool.md`.

### MCP (requires `[tool.mcp]`)

| Method | Status | Notes |
|---|---|---|
| `tool.mcp.list_servers` | live | Returns operator-declared servers with `status="configured"`. |
| `tool.mcp.list_tools` | live | Live `tools/list` for stdio transport; declared list for http transport. |
| `tool.mcp.invoke` | scaffold (http) / live (stdio) | Stdio: live subprocess dispatch. HTTP: returns `RuntimeNotConnected` until D-009. |

See `docs/mcp-tool.md`.

### Utility (always registered)

| Method | Status | Notes |
|---|---|---|
| `tool.screen` | live | Cross-platform host-screen PNG capture. Disabled by default (`[tool.screen] enabled = false`). |
| `tool.ask_human` | live | Post a question to an operator channel and await reply. Default timeout 300 s. |
| `memory.session_search` | live | Proxy from tool node to the memory peer's `task.session_search` capability. |

---

## Router node (`crates/relix-runtime/src/nodes/router.rs`)

Active only when `[controller] role = "router"`. Same `relix-controller` binary; never holds provider keys or makes LLM calls.

| Method | Status | Risk | Notes |
|---|---|---|---|
| `router.heartbeat` | live | Low | Controller-only push; registers/updates peer + caps + groups. CBOR wire format. |
| `router.network_summary` | live | Low | Operator-facing mesh overview (peers, active sessions, uptime); `org_filter` substring. CBOR wire format. |
| `router.session_list` | live | Low | Operator-facing session browser; `status_filter` + `limit` + `offset` pagination. CBOR wire format. |
| `router.log` | live | Low | Controller-only push; bounded 10 000-line in-memory ring. CBOR wire format. |

Background loops (router role only): stale-peer reaper every 30 s (marks `healthy=false` after 90 s of no heartbeat); session reaper every 300 s (removes `completed`/`failed` sessions past `session_ttl_secs`, default 1800 s).

See `configs/router-node.toml` and `configs/policies/router.toml`.

---

## Observability / Metrics (`crates/relix-runtime/src/metrics/`)

Registered on the coordinator node when `[metrics] enabled = true`.

| Method | Status | Notes |
|---|---|---|
| `metrics.agents` | live | Per-agent summary list over configurable window (default 24 h). |
| `metrics.agent_summary` | live | Detailed summary for one agent: invocations, success rate, P50/P95/P99 latency, cost. |
| `metrics.method_breakdown` | live | Per-capability breakdown for one agent over a time window. |
| `metrics.timeseries` | live | Time-bucketed invocation + cost series for one agent (default bucket 5 min). |
| `metrics.alerts_active` | live | Currently active alerts from the `AlertEngine`. |
| `metrics.cost_report` | live | Cost-per-agent-per-method report for a time window. |
| `metrics.cost_baselines` | live | Recent cost-baseline windows from the spike detector store. |
| `metrics.ask_human_baselines` | live | Recent `tool.ask_human` rate baselines from the spike detector store. |
| `metrics.cost_spike_history` | live | Recent cost-spike events recorded by the `CostSpikeDetector`. |
| `observability.active_alerts` | live | Active alert list (richer JSON than `metrics.alerts_active`; requires `[metrics]` + `observability` wiring). |
| `observability.alert_history` | live | Chronicle of fired/recovered alert events: `{limit?, agent?}` → JSON rows. |
| `observability.health_summary` | live | Agent health dashboard: 100-point score per agent (40% error-rate / 30% P95 latency / 20% confidence / 10% budget utilization). |

---

## Knowledge sharing (`crates/relix-runtime/src/{knowledge,training,confidence}/`)

### Knowledge (`RELIX-7.16`) — registered on coordinator when `[[knowledge.groups]]` is non-empty

| Method | Status | Notes |
|---|---|---|
| `knowledge.share` | live | Transfer Layer 3 observations between agents in the same group. Ed25519-signed payload; TrustChecker gate (8 checks). |
| `knowledge.list_shared` | live | Cursor-paginated list of shared records received by an agent (default page 100, max 1000). |
| `knowledge.group_broadcast` | live | Fan-out share to all members of a named group. |
| `knowledge.groups` | live | List configured sharing groups and their members. |
| `knowledge.revoke` | live | Revoke previously shared records by id (marks copies with `revoked` tag). |
| `knowledge.recall` | live | Revoke all copies of a source agent's observations from target agents. Ownership-checked. |
| `knowledge.accept_shared` | live | Cross-node receiver endpoint: accepts `SignedSharePayload` from a remote node. Fail-closed: unregistered source nodes rejected by default (`allow_unbound_sources = false`). |
| `knowledge.autoshare_stats` | live | Lifetime + per-tick stats for the `AutoShareTask` background propagator. |

### Training (`RELIX-7.15`) — registered on coordinator when `[training] enabled = true`

| Method | Status | Notes |
|---|---|---|
| `training.list_interactions` | live | Paginated interaction list with quality, agent, model, and date filters. |
| `training.get_interaction` | live | Full interaction record by id. |
| `training.export` | live | Export quality-filtered interactions in `openai`, `anthropic`, `generic`, or `raw_json` format to `export_dir`. |
| `training.score_interaction` | live | Compute and persist the 5-factor quality score for one interaction. |
| `training.stats` | live | Aggregate stats: total, exported, score distribution, per-agent and per-model breakdowns. |
| `training.delete_interaction` | live | Hard-delete one interaction record (permanent; no soft-delete). |
| `training.pii_scan` | live | Scan an interaction's fields for PII spans (regex-based). |
| `training.anonymize_preview` | live | Preview PII anonymization of an interaction record without mutating it. |

### Confidence (`RELIX-7.19`) — registered on coordinator when `[confidence] enabled = true`

| Method | Status | Notes |
|---|---|---|
| `confidence.policy_list` | live | List all configured `ConfidencePolicy` entries as JSON. |
| `confidence.score_history` | live | Rolling-window snapshot for one `(agent, method)`: call count, error rate, P50/P95/P99 latency, avg confidence. |
| `confidence.reset_history` | live | Clear rolling-window state for one `(agent, method)` pair, or all methods for one agent. |
| `confidence.self_consistency_stats` | live | Process-wide self-consistency sampling stats (trigger count, average score, cost guard state). **Note: registered in coordinator but NOT listed in `confidence_capability_descriptors()` — this capability will not appear in the node manifest's capability list.** |

---

## Planning and Workflow (`crates/relix-runtime/src/{planning,workflow}/`)

### Planning (`RELIX-7.24`) — registered on coordinator

| Method | Status | Notes |
|---|---|---|
| `planning.list_agents` | live | List agents known to the `AgentCapabilityRegistry`. |
| `planning.find_agents` | live | Score and rank agents by task-description keyword match. |
| `planning.validate_spec` | live | Parse and validate a natural-language plan spec; return parsed `PlanSpec` with complexity score. |
| `planning.create_plan` | live | Full 5-stage pipeline (parse → orchestrate → conflict-resolve → critic-loop → [approval gate] → execute): JSON `{spec, max_agents?, dry_run?, require_approval?}`. |
| `planning.orchestrator_status` | live | Current orchestrator configuration and activation state. |
| `planning.approve_plan` | live | Approve a gated plan (verifies spec signature before execution). Requires `approval_store` configured. |
| `planning.reject_plan` | live | Reject a gated plan. Requires `approval_store` configured. |
| `planning.list_approvals` | live | List pending/approved/rejected/expired plan approvals. |
| `planning.get_approval` | live | Read one plan approval record by `spec_id`. |
| `planning.verification_log` | live | Per-step verification results for an executed plan. |
| `planning.export_spec` | live | Export a `PlanSpec` as JSON or markdown. |

### Workflow — registered on coordinator

| Method | Status | Notes |
|---|---|---|
| `workflow.run` | live | Execute a named workflow by file (`<data_dir>/workflows/<name>.workflow`); blocking. |
| `workflow.run.stream` | live | Streaming variant; emits `WorkflowEvent` JSON per step (`started`, `step_started`, `step_completed`, `step_failed`, `finished`, `cancelled`). |
| `workflow.list` | live | List available workflows in the catalog directory. |
| `workflow.status` | live | Read one execution record from the chronicle by `execution_id`. |
| `workflow.validate` | live | Parse and validate a workflow YAML string without executing. |
| `workflow.reload` | live | Clear the workflow file cache (forces re-read from disk on next run). |

---

## Execution infrastructure (`crates/relix-runtime/src/nodes/execution/`)

Registered on coordinator when `[execution.gateway]` is configured.

| Method | Status | Notes |
|---|---|---|
| `execution.evidence` | live | Query the per-action evidence store: JSON `{action_id?, actor_id?, limit?}` (default limit 20, cap 200) → JSON `{records:[…], count}`. |
| `execution.rollback` | live | Unwind a transaction: JSON `{transaction_id}` → `RollbackResult` JSON. Tier-A actions use compensating tools; Tier-B surfaces rollback plan; Tier-C logged as errors. |
| `execution.transaction_get` | live | Read all actions for a transaction by id → JSON `{transaction_id, actions, count}`. |

---

## Security gating (`crates/relix-runtime/src/{approval,credentials}/`)

### Approval delivery — registered on coordinator when `[approval.delivery]` is configured

| Method | Status | Notes |
|---|---|---|
| `approval.delivery_status` | live | Read one approval delivery row by `approval_id`. |
| `approval.deliver` | live | Dispatch a new approval request (upsert + channel send). |
| `approval.record_decision` | live | Operator approve/reject/expire; checks `authorized_approvers` OR `operator`/`admin` role. |
| `approval.failed_deliveries` | live | List `delivery_failed` rows (default limit 50, cap 500). |
| `approval.list_pending` | live | List `pending` rows (default limit 50, cap 500). |

### Credentials (`RELIX-7.30 P2`) — registered when `[credentials] enabled = true`

| Method | Status | Risk | Notes |
|---|---|---|---|
| `credentials.store` | live | High | Encrypt + insert a credential (AES-256-GCM / Argon2id vault). |
| `credentials.get` | live | High | Decrypt + return credential value; caller must equal `owner_agent` (ownership-only gate; no role bypass). |
| `credentials.rotate` | live | High | Replace encrypted value with new material; bumps version. |
| `credentials.revoke` | live | Medium | Flip `revoked = 1`; credential is permanently inaccessible after revocation. |
| `credentials.list` | live | Low | Return credential summaries (names, metadata) — no encrypted blobs. |
| `credentials.audit` | live | Low | Per-credential audit trail (stored/accessed/rotated/revoked/kdf_migrated events). |

---

## Session identity (`crates/relix-runtime/src/identity/`)

Registered when `[identity.session] enabled = true`.

| Method | Status | Notes |
|---|---|---|
| `identity.issue_token` | live | Mint a HMAC-SHA256 session token (CBOR/base64url) for a `(session_id, agent_name, tenant_id)`. |
| `identity.verify_token` | live | Verify and touch a session token (10-step pipeline including constant-time HMAC compare + SQLite atomic touch). |
| `identity.revoke_token` | live | Revoke all tokens for a `session_id`. |
| `identity.active_tokens` | live | List active (non-revoked, non-expired) tokens with optional `agent_name` filter. |
| `identity.research` | live | 5-stage identity research pipeline (web search → LLM synthesis → approval gate → memory write). Requires `[identity.research] enabled = true`. |

---

## PII gate (`crates/relix-runtime/src/nodes/pii_gate_coordinator.rs`)

Registered on coordinator when `[mesh_pii] enabled = true`.

| Method | Status | Notes |
|---|---|---|
| `pii.scan_stats` | live | Aggregate PII detection stats for a time window (default 24 h, max 2160 h / 90 days). |
| `pii.recent_events` | live | Recent PII detection audit events (default limit 50, max 1000); optional `method` filter. |

---

## Channel nodes

### Telegram (`crates/relix-runtime/src/nodes/telegram/`)

| Method | Status | Notes |
|---|---|---|
| `telegram.status` | live | Bot online state + identity + ring counters (pipe-delimited). |
| `telegram.messages_recent` | live | Last N inbound messages from bounded ring (cap 200), newest-first. |
| `telegram.send` | live | Outbound message: JSON `{chat_id, text}` → `{"ok":true}`. |
| `telegram.approval_send` | live | Dispatch an approval notification to a Telegram chat (PART 8). |
| `telegram.health` | live | `ChannelHealth` snapshot (FIX 49): mode, last-poll, error state. |
| `telegram.webhook_update` | live | Inbound Telegram update from a webhook endpoint (optional; requires `mode = "webhook"`). |

### Discord (`crates/relix-runtime/src/nodes/discord/`)

| Method | Status | Notes |
|---|---|---|
| `discord.status` | live | Bot online state + identity + ring counters (pipe-delimited). |
| `discord.messages_recent` | live | Last N inbound messages from bounded ring (cap 200), newest-first. |
| `discord.send` | live | Outbound message: JSON `{channel_id, text}` → `{"ok":true}`. Splits at 1900 chars. |
| `discord.approval_send` | live | Dispatch an approval notification to a Discord channel (PART 8). |
| `discord.health` | live | `ChannelHealth` snapshot (FIX 49). |

### Slack (`crates/relix-runtime/src/nodes/slack/`)

| Method | Status | Notes |
|---|---|---|
| `slack.status` | live | Bot online state + identity + `team_id` + ring counters (pipe-delimited). |
| `slack.messages_recent` | live | Last N inbound messages from bounded ring (cap 200), newest-first. |
| `slack.send` | live | Outbound message: JSON `{channel, text}` → `{"ok":true}`. Converts `**bold**` to mrkdwn. |
| `slack.approval_send` | live | Dispatch an approval notification to a Slack channel (PART 8). |
| `slack.health` | live | `ChannelHealth` snapshot (FIX 49). |

### Email (`crates/relix-runtime/src/nodes/email/`)

| Method | Status | Notes |
|---|---|---|
| `email.status` | live | SMTP + IMAP link status, counters, error strings, last timestamps (pipe-delimited). |
| `email.messages_recent` | live | Last N inbound messages from bounded ring (cap 200), newest-first; preview 200 chars. |
| `email.send` | live | Outbound email: JSON `{to, cc, bcc, reply_to, subject, body, html, in_reply_to, references, attachments}` → `{"message_id":"…"}`. Supports DKIM signing. Max 26 MiB. |
| `email.send_template` | live | Template-based send: JSON `{template_name, to, variables, …}` → `{"message_id":"…","template":"…"}`. Built-ins: `welcome`, `reset_password`, `task_completed`, `task_failed`. |
| `email.approval_send` | live | Dispatch an approval notification via email (PART 8). |

---

## Bridge surface (`crates/relix-web-bridge/src/`)

The bridge translates HTTP → coord capabilities. Notable operator-facing endpoints:

| Endpoint | Notes |
|---|---|
| `GET /v1/tasks/*` | Read-side projections of the coord ledger |
| `GET /v1/tasks/:id/todos` / `PUT` / `PATCH` (PH-DASH2) | Per-task todo CRUD |
| `GET /v1/tasks/stuck` (H6) | Stuck-running projection |
| `GET /v1/tasks/events/recent` / `/stream` (M67/M73) | Cross-task firehose + SSE |
| `GET /v1/providers/health` (PH-WAVE2K) | Consolidated AI-stack snapshot |
| `POST /v1/providers/route_test` (PH-ROUTER-PREVIEW) | Preview `HealthAwareRouter` pick for a candidate list |
| `GET /v1/config/providers` | Per-provider redacted status |
| `GET /v1/memory/curator/status` | Curator scheduler state (bridge proxy for `memory.curator_status`) |
| `GET /v1/mcp/servers?peer=<alias>` (PH-BRIDGE-MCP) | Bridge HTTP proxy → `tool.mcp.list_servers` |
| `GET /v1/mcp/tools?peer=<alias>&server_id=<id>` (PH-BRIDGE-MCP) | Bridge HTTP proxy → `tool.mcp.list_tools` |
| `POST /v1/mcp/invoke` (PH-BRIDGE-MCP-INVOKE) | Bridge HTTP proxy → `tool.mcp.invoke` (returns 502 until D-009) |
| Route-latency tracing middleware (H15) | Structured log field per request |
| Operator intervention audit (M57 + H9) | All mutating routes recorded; H9 redacts |

## Operator UI (`crates/relix-web-bridge/src/dashboard.html`)

Since the v0.3.0 rebuild the console is a single-page app with a
sidebar of panels and no `#/...` hash routes. The current build has
twenty-two: Overview, Tasks, Scheduled Jobs, Chat, Memory, Approvals,
Skills, Sessions, Reasoning, Credentials, Identity, Cost & Metrics,
Observability, Policy Denials, Multi-Tenant, Planning, Workflows,
Email, Plugins, MCP Servers, Configuration, and Logs. The `SECTIONS`
array in `dashboard.html` is the source of truth; see
[operator-guide.md](operator-guide.md) for a per-panel breakdown.
Topology, health, and cost roll up into Overview. There is no
Capabilities, Topology, Telegram, or Providers page; provider config
lives under Configuration, and capability/topology data is on the
HTTP API (`/v1/capabilities`, `/v1/topology`).

## CLI (`crates/relix-cli/`)

| Command | Notes |
|---|---|
| `relix-cli identity` | Mint / inspect identity bundles |
| `relix-cli task` | Operate the coord ledger |
| `relix-cli capability ls` | Per-peer manifest dump |
| `relix-cli capability ls --risk <tier[+]>` (PH-CAP-RISK3) | Filter by risk tier; `+` means at-or-above |
| `relix-cli topology show / health` | Topology + bridge health |
| `relix-cli flow-run` | SOL/YAML flow execution |
| `relix-cli ops providers-health` (PH-WAVE2L) | Consolidated AI-stack snapshot |
| `relix-cli ops capabilities` (PH-DASH3-CLI) | Mesh-wide capability list |
| `relix-cli ops stuck` (PH-OPS-STUCK) | H6 stuck-running projection |
| `relix-cli ops events` (PH-OPS-EVENTS) | H2 firehose snapshot |
| `relix-cli ops route-test` (PH-ROUTER-PREVIEW-CLI) | Preview `HealthAwareRouter` pick |
| `relix-cli router status` (PH-ROUTER-NODE) | Router mesh overview via `router.network_summary` |
| `relix-cli router peers` (PH-ROUTER-NODE) | Per-peer table from `router.network_summary` |
| `relix-cli router sessions` (PH-ROUTER-NODE) | Session browser via `router.session_list` |
| `relix-cli mcp servers --peer …` (PH-MCP-CLI) | Lists MCP servers on a tool node |
| `relix-cli mcp tools --peer … --server-id …` (PH-MCP-CLI) | Lists declared tools for a server |
| `relix-cli terminal sessions --peer …` (PH-TERM-CLI) | Live in-flight terminal sessions |
| `relix-cli terminal audit --peer … [--max N]` (PH-TERM-CLI) | Completion ring snapshot |
| `relix-cli terminal cancel --peer … --session-id …` (PH-TERM-CLI) | Cooperative cancel |
| `relix-cli web blocklist` | Snapshot of `[tool] blocked_hosts` |
| `relix-cli ping` | Direct libp2p ping to a peer |

---

## Per-feature docs

When a capability has subsystem-specific contracts, see the dedicated doc:

- `docs/browser-tool.md` — CW4 honesty contract + Playwright roadmap
- `docs/mcp-tool.md` — CW5 honesty contract + live-client roadmap
- `docs/tool-node-security.md` — SSRF, terminal allowlist, jail discipline
- `docs/chronicle-retention.md` — Chronicle compaction design
- `docs/event-contract.md` — Chronicle event_type vocabulary
- `docs/event-vocabulary.md` — H2 one-line summary projection rules
- `docs/security.md` — Top-level admission pipeline
- `docs/bridge-invariants.md` — What the bridge MAY / MUST NOT do
- `docs/operator-guide.md` — Logs + common failures + CLI surface
- `docs/agent-permissions.md` — Agent gate categorical checks + approval flow
- `docs/agent-memory.md` — Persistent agent + user memory blobs
- `docs/messaging.md` — Agent-to-agent mail-drop protocol

## Internal-only

These are operator-internal docs, not part of the public catalog:

- `docs/internal/continuation-state.md` — Autonomous-run handoff
- `docs/internal/decisions-pending.md` — Open operator decisions
- `docs/internal/hermes-capability-map.md` — Hermes parity inventory
