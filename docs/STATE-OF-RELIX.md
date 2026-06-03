# STATE OF RELIX

**Audit timestamp:** 2026-05-21 (original) · reconciled against 0.4.1 codebase 2026-06-01
**Purpose:** read-only snapshot of what exists, what's partial, what's
proposal-only. Written for someone who has never read this codebase.

---

## 1. WHAT RELIX IS

Relix is a **mesh of peer processes** that an operator runs locally on
one machine to coordinate multiple AI agents and tools through a
**signed, audited, policy-gated dispatch pipeline**. Every call between
peers carries a signed identity bundle, passes through an admission
pipeline (identity verify → policy → handler → audit), and writes an
entry to a per-node hash-chained audit log. Orchestration lives in
small hand-written **SOL flow files** (a tiny imperative DSL with a
`remote_call(peer, method, args)` primitive). There is no central
gateway — the HTTP bridge that fronts OpenAI-compatible clients is just
another peer on the mesh. The whole thing is honest about what it
**does not** do: no DHT discovery,
no rate limiting beyond the BudgetEnforcer spend caps (no per-method token-bucket throttling).
As of 0.4.1 subprocess plugins ARE supported (`node_type = "plugin_host"`);
dynamic WASM loading is still absent. The differentiating posture is
operator-facing transparency — every dispatched call, denial, retry,
and chronicle event is queryable by both a dashboard and CLI, and the
codebase contains a docs/current-limitations.md and per-feature
"honesty contract" notes that name exactly what is scaffold vs real.

---

## 2. HOW THE MESH WORKS

### 2.1 Process model

Each peer is an OS process (`relix-controller` binary) with its own
Ed25519 identity, its own libp2p listen address, and its own dispatch
bridge. The HTTP front (`relix-web-bridge` binary) is also a peer — it
just additionally speaks HTTP for OpenAI-compatible clients. There is
no central service.

### 2.2 Transport

`/relix/rpc/1` over libp2p (TCP + Noise XK + Yamux). Wire format is
CBOR-encoded `RequestEnvelope` / `ResponseEnvelope` carrying:
- caller's signed `IdentityBundle`
- method name
- opaque argument bytes
- deadline

### 2.3 Node types

There is **one binary** (`relix-controller`) whose behavior is selected
by `[controller] node_type` in the config file. Six node types exist
today:

| `node_type` | Purpose | Backing store |
| --- | --- | --- |
| `memory` | Per-session chat memory; FTS5 search | SQLite + FTS5 |
| `ai` | Provider-agnostic chat completion (`ai.chat`) | OpenAI / Anthropic / OpenRouter / xAI / Gemini / Ollama-compatible local / `mock` |
| `tool` | Web fetch, fs jail, terminal exec, browser automation, MCP registry, PDF parse, text chunk | reqwest + jailed local fs + portable-pty + headless_chrome / webdriver |
| `coordinator` | Durable Task ledger + per-task chronicle | SQLite |
| `router` | Mesh observability + heartbeat aggregator (control plane, NOT request routing) | in-memory rings |
| (bridge) | HTTP front + OpenAI shim + dashboard host | — |

The bridge is technically not a `node_type` — it's its own binary
(`relix-web-bridge`).

### 2.4 Admission pipeline

Every inbound call on every node runs the same steps (in
`crates/relix-runtime/src/dispatch/mod.rs`):
1. Decode envelope
2. Validate identity bundle (org-root signed Ed25519)
3. Deadline check
4. PolicyEngine evaluate (`[admit]` groups + per-method `[[rules]]`)
5. Dispatch to registered handler
6. Append audit record (signed, hash-chained)

Identity, policy, audit are in `relix-core`; the bridge that chains them
into the pipeline is in `relix-runtime::dispatch`. There is no plugin
or trusted path that bypasses these steps.

### 2.5 SOL — the orchestration DSL

SOL is a small imperative DSL — `let x: str = ...`, `print(...)`,
`return ...`, `function start() -> str { ... }`. The only mesh-aware
primitive is **`remote_call(peer_alias_or_capability, method, args)`**.
SOL strings are taken verbatim (no escapes), and the per-method
argument convention is pipe-delimited (`session_id|prompt|history`).

Six flow files ship today (`flows/`):
- `ping.sol` — single `remote_call("controller", "node.health", "")`
- `chained_health.sol` — two health calls (memory + ai), demonstrates ordering
- `memory_demo.sol` — write a user turn → write assistant turn → read history
- `chat.sol` — full chat: persist user → read history → ai.chat → persist assistant
- `chat_template.sol` — bridge-rendered template (substitutes session + message)
- `chat_with_tool.sol` — chat with a `tool.web_fetch` step

SOL is the **only place** where orchestration ordering lives. The Rust
code in the bridge selects which `.sol` to render; it does not encode
"persist before fetch" anywhere. That's an architectural invariant.

Runtime details:
- VM is synchronous (no yield mid-flow — see `docs/replay-model.md`).
- Per-flow event log is append-only, signed, hash-chained
  (`crates/relix-core/src/eventlog.rs`).
- Args are `String` (SIMP-016 — typed CDDL is deferred to Gate 2).

---

## 3. EVERY CAPABILITY THAT EXISTS TODAY

Inventory derived from grepping `pub fn descriptor_*` and
`CapabilityDescriptor::unary("...")` across the workspace. **Real** =
handler runs and returns useful output. **Scaffold** = handler exists
but returns `BackendNotConnected` / `RuntimeNotConnected` by design
until a backend is configured. **Built-in (every node)** = registered
on every controller regardless of `node_type`.

### 3.1 Built-in (every controller)

| Method | Status | What it does |
| --- | --- | --- |
| `node.health` | Real | Returns node id, uptime, build hash, listening port |
| `node.manifest` | Real | Returns the full `NodeManifest` with descriptors |
| `node.dispatch.stats` | Real | Per-capability invocation + latency snapshot (W2-006b) |
| `node.policy.simulate` | Real | "What if" — evaluate caller+method without invoking (W2-007a) |
| `node.policy.recent_denials` | Real | Bounded ring (256) of recent policy denies (W2-007d) |

### 3.2 `memory` node

| Method | Status | What it does |
| --- | --- | --- |
| `memory.write_turn` | Real | Persist `session_id\|role\|body` into SQLite |
| `memory.recent_for_session` | Real | Read last N turns oldest-first (default 10) |
| `memory.search_turns` | Real | FTS5 query across all turns (was `memory.search` before vector memory landed) |
| `memory.embed` | Real | Embed one chunk into the per-subject vector store; idempotent by content hash. Requires `[memory.embedding_peer]` |
| `memory.search` | Real | Semantic search over a subject's embeddings (cosine, top-K up to 20) |
| `memory.embed_all` | Real | Re-embed all persistent-memory entries for one subject (split on `§`; idempotent) |
| `memory.agent_read` | Real | Read agent + user persistent memory for a subject |
| `memory.agent_write` | Real | Add / replace / remove / read persistent memory |
| `memory.agent_curate` | Real | Curator (LLM-driven memory consolidation) |
| `memory.curator_status` | Real | Curator scheduler status snapshot |
| `memory.session_search` | Real | Full-text search across `chat.user_turn` / `chat.assistant_turn` chronicle events. Thin proxy onto coordinator's `task.session_search`. Requires `[memory.curator]` coord_peer to forward. See `docs/agent-memory.md` |

Vector memory is documented in [`docs/vector-memory.md`](./vector-memory.md).
SQLite store, cosine similarity in Rust, full table scan (good for
the hundreds-per-subject row count the cap budgets allow). Switch
to `sqlite-vec` or HNSW is a local change to
`nodes/memory/embeddings.rs` if the scan ever hurts.

### 3.3 `ai` node

| Method | Status | What it does |
| --- | --- | --- |
| `ai.chat` | Real | Provider-routed completion via `[ai] provider = ...` |
| `ai.embed` | Real | Batch text embedding. Mock provider returns deterministic 8-dim vectors; OpenAI-compatible hits `/v1/embeddings` |

Provider routing supports `mock`, `openai`, `anthropic`, `openrouter`,
`xai`, `gemini`, and a `local` Ollama-compatible base URL. Provider
keys live in the bridge's `bridge-secrets.toml` (operator sets via the
dashboard config page). A separate **HealthAwareRouter** scaffold
exists (`POST /v1/providers/route_test`) for previewing provider
selection without making a chat call.

### 3.4 `tool` node — filesystem

All scoped to operator-configured jail roots in `[tool] roots = [...]`.

| Method | Status | What it does |
| --- | --- | --- |
| `tool.read_file` | Real | Read text file inside a jail |
| `tool.write_file` | Real | Overwrite + audit-ring entry |
| `tool.append_file` | Real | Append + audit-ring entry |
| `tool.patch` | Real | Old/new line replace |
| `tool.patch_preview` | Real | Dry-run preview |
| `tool.fuzzy_replace` | Real | Whitespace-tolerant text replace |
| `tool.search_files` | Real | Recursive search with `glob` mode (`*` / `**` / `?`) |
| `tool.list_dir` | Real | List one directory level |
| `tool.fs.tree` | Real | Recursive tree with depth cap |
| `tool.fs.stat` | Real | size / mtime / mode |
| `tool.binary_sniff` | Real | Detect binary via NUL-byte heuristic |
| `tool.fs.audit_recent` | Real | Per-jail mutation ring (capacity 256) |

### 3.5 `tool` node — web

SSRF-guarded; obeys `[tool] blocked_hosts`.

| Method | Status | What it does |
| --- | --- | --- |
| `tool.web_fetch` | Real | GET; text-only response; cap on body size |
| `tool.web_get` | Real | Alias / extended GET path |
| `tool.web_search` | Real | Provider-backed search (configurable) |
| `tool.web_extract` | Real | HTML → text / markdown structural conversion |
| `tool.web.post` | Real | POST surface (separately gated) |
| `tool.web.robots_check` | Real | robots.txt admittance check |
| `tool.web.blocklist_summary` | Real | Read-only view of `[tool] blocked_hosts` |

### 3.6 `tool` node — terminal

Allowlisted commands only (`[tool.terminal] allowed = [...]`).

| Method | Status | What it does |
| --- | --- | --- |
| `tool.terminal.run` | Real | One-shot allowlisted command |
| `tool.terminal.spawn` | Real | Long-running spawn with session id |
| `tool.terminal.tail` | Real | Polling cursor over a running session's output |
| `tool.terminal.cancel` | Real | Cooperative cancel of a running session |
| `tool.terminal.sessions` | Real | Live registry of running sessions |
| `tool.terminal.audit_recent` | Real | Completion ring (capacity 256) |
| `tool.terminal.shell.open` | Real (PTY-gated) | Open an interactive shell session — requires `terminal-pty` feature |
| `tool.terminal.shell.input` | Real (PTY-gated) | Feed input to a shell session |
| `tool.terminal.shell.control` | Real (PTY-gated) | Send control signal (resize, signal) |
| `tool.terminal.shell.close` | Real (PTY-gated) | Close a shell session |

### 3.7 `tool` node — browser

Selected by `[tool.browser] backend = "none" | "headless_chrome" | "playwright" | "webdriver"`.

| Method | `headless_chrome` | `webdriver` | `playwright` | `none` |
| --- | --- | --- | --- | --- |
| `open_session` / `close_session` / `list_sessions` | Real | Real | Real | Real (id-only) |
| `navigate` / `get_text` / `screenshot` | Real | Real | Scaffold | BackendNotConnected |
| `click` / `type_text` / `wait_for_selector` | Real | Real | Scaffold | BackendNotConnected |
| `capture_read` (read saved PNG) | Real (operator dir) | Real (operator dir) | Real | Real (when configured) |

Failure-screenshot capture (`screenshot_on_failure_dir`) is wired on
the HC and WD backends. The `capture_read` method serves the PNG bytes
back to the dashboard via `/v1/browser/captures/:filename`.

### 3.8 `tool` node — MCP registry

| Method | Status | What it does |
| --- | --- | --- |
| `tool.mcp.list_servers` | Real (registry only) | Operator-declared servers from config |
| `tool.mcp.list_tools` | Real (per server) | Operator-declared tools per server |
| `tool.mcp.invoke` | Stdio: Real / HTTP: scaffold | Spawn an MCP server over stdio and invoke a tool; HTTP transport returns `RuntimeNotConnected` |

### 3.9 `tool` node — other

| Method | Status | What it does |
| --- | --- | --- |
| `tool.pdf` | Real | Extract text from a PDF |
| `tool.text.chunk` | Real | Generic chunker (paragraph > sentence > word > char) |

### 3.10 `coordinator` node — task ledger

Capabilities follow the convention `task.*`. Most are CRUD-shaped.

| Method | Status |
| --- | --- |
| `task.create` / `task.update` / `task.get` / `task.list` / `task.count` / `task.list_cursor` | Real |
| `task.event` / `task.events` | Real |
| `task.attempts` / `task.recent_edges` / `task.edges` | Real |
| `task.lineage` (single-task envelope) | Real |
| `task.lineage` (graph BFS) — exposed as `coord` method, bridge surfaces at `/v1/tasks/:id/lineage_graph` | Real |
| `task.retry` / `task.replay` | Real |
| `task.recover` | Real (operator-driven) |
| `task.note` / `task.mark_investigation` | Real |
| `task.export` / `task.compact_events` (dry-run) | Real |
| `task.session_export` / `task.session_search` | Real |
| `task.spawned_child` / `task.delegated_to` / `task.awaiting` | Real chronicle events |

Plus a long list of chronicle event types (`task.thrash_detected`,
`task.terminal_summary`, `task.attempt_orphan_closed`,
`task.retry_requested` / `_exhausted` / `_suppressed`,
`task.pause_requested` / `_observed`, `task.resume_*`, `task.freeze_*`,
`task.investigation_marked` / `_cleared`, `task.operator_note`,
`task.replayed_from`, `task.failed`, `task.completed`,
`task.cancelled`, `task.interrupted`, `task.attempt_started` /
`_finished`, `flow.started`, `capability.invoked`).

The coordinator ships an **agent-to-agent messaging** surface
on the same db — five `msg.*` capabilities (`send` / `inbox` /
`read` / `thread` / `delete`) over an `agent_messages` table.
Distinct from delegation: no task is created per message, just
one `msg.sent` chronicle entry on a `msg-bookkeeping-system`
task that excludes the body for audit redaction. Messages
auto-expire after `ttl_secs` (default 24 h) via a 5-minute
sweeper. Thread access is participant-gated; soft delete
flips status to `expired` so audit retains the row. See
[messaging.md](messaging.md).

The coordinator ships a **delegation** surface that lets one
agent spawn another as a subtask. Four capabilities
(`delegate.spawn` / `result` / `cancel` / `list`) on top of the
existing `task_edges` table — `delegate.spawn` creates a child
task with `origin_surface = "delegation"`, records a
`delegated_to` edge (re-using the existing `record_delegated`
producer), and flips the parent to `awaiting_input` with a
`task.awaiting` chronicle event. A 5 s background executor
picks up pending children (`max_concurrent` semaphore default 5,
`max_job_secs` timeout default 300), dispatches `ai.chat` with
the goal + context, then writes `delegate.completed` /
`delegate.failed` on the child and `delegate.child_completed`
on the parent before resuming the parent to `running`. Depth
cap (default 3) is enforced twice — against the caller's
claimed `depth` AND against an independent walk of the
`delegated_to` ancestor chain. Honest contract: `delegate.spawn`
returns the `child_task_id` immediately (not a blocking call);
agent loops poll `delegate.result`. See
[delegation.md](delegation.md).

The coordinator also ships a **cron scheduler** that fires durable
scheduled jobs. Six capabilities (`cron.create` / `list` / `get` /
`update` / `delete` / `trigger`) backed by a `cron_jobs` SQLite
table sharing the coordinator's database. A 30 s background loop
scans for due jobs (enabled rows with `next_run_at <= now`), creates
a `cron:<name>` task with `origin_surface = "scheduler"`, writes a
`cron.job_fired` chronicle event, dispatches `ai.chat` against the
configured peer with a per-job `max_job_secs` timeout, then writes
a `cron.job_result` event with the AI reply preview. Hardening:
semaphore caps concurrent fires (default 3), pile-up guard skips
the next fire when the previous task is still `running`, one-shot
jobs are auto-disabled after their first fire. Schedule formats:
duration (`30m`, `2h`, `1d`, `7d`), 5-field cron (`0 9 * * 1`),
RFC 3339 one-shot (`2026-06-01T09:00:00Z`). See
[scheduler.md](scheduler.md) for the full design.

### 3.11 `router` node — control plane

CBOR-encoded (not pipe-delimited like the rest).

| Method | Status | What it does |
| --- | --- | --- |
| `router.heartbeat` | Real | Controllers push liveness + caps every 60s |
| `router.network_summary` | Real | Operator-facing mesh overview |
| `router.session_list` | Real | Cross-peer session browser |
| `router.log` | Real | Controllers push structured log lines (bounded ring 10k) |

Reaper loops: stale-peer flip (90s), expired-session drop (300s).

### 3.12 What is **not** a capability (as of 0.4.1)

All three chat channels (Telegram, Discord, Slack) and the email channel
are fully wired. Each registers `<channel>.status`, `<channel>.messages_recent`,
`<channel>.send`, `<channel>.health`, and `<channel>.approval_send`
capabilities on their respective controller nodes. The scaffold-only
note from the original audit no longer applies.

---

## 4. WHAT THE DASHBOARD SHOWS

**Update:** The operator dashboard was rebuilt in v0.3.0 into a single
self-contained `dashboard.html` (CSS + JS inline, no external deps, no
CDN). The current build carries **22 top-level sections**. The
subsection inventory below reflects the pre-rebuild structure and is
annotated where sections have changed materially.

Served by `relix-web-bridge` at `GET /dashboard` as a single static
HTML file with inline JS (no build step). Twenty-two top-level
sections, each a `data-section="<id>"` panel selected from the sidebar
`SECTIONS` array in `crates/relix-web-bridge/src/dashboard.html`. There
is no `#/...` hash routing and no `data-page` attribute.

> **Superseded subsections.** The current twenty-two panels are
> Overview, Tasks, Scheduled Jobs, Chat, Memory, Approvals, Skills,
> Sessions, Reasoning, Credentials, Identity, Cost & Metrics,
> Observability, Policy Denials, Multi-Tenant, Planning, Workflows,
> Email, Plugins, MCP Servers, Configuration, and Logs. The §4.1 to
> §4.10 subsections below describe the pre-rebuild page set
> (`#/tasks`, `#/topology`, `#/capabilities`, `#/mcp`, `#/fsaudit`,
> `#/termaudit`, `#/browser`, `#/metrics`, `#/providers`) and use a
> `#/...` hash routing that no longer exists. Tasks, Scheduled Jobs
> (cron), Policy Denials, and MCP Servers are now real panels; there
> is still no Topology, Capabilities, fsaudit, termaudit, browser,
> Metrics, or Providers page, and that data lives on the HTTP API and
> the `relix` CLI. See [operator-guide.md](operator-guide.md) for the
> per-panel breakdown.

### 4.1 `#/overview`

- Status bar (uptime, coordinator reachability, peer counts).
- Last-recovery banner when the C1b recovery scan flipped anything to `interrupted`.
- H6 stuck-task card (auto-hides when count=0).
- H11 ops-health KPI tiles: stuck / thrash / orphan / terminal / redaction.
- H13 top-5 event_type histogram from the firehose ring.
- Global SSE-fed firehose (200-entry ring, filterable substring).

### 4.2 `#/tasks`

- Quick-filter chips: `all / running / failed / interrupted / completed / stuck? / investigating`.
- W2-003f time-window chips: `all-time / last 1h / last 6h / last 24h` (URL-persisted via `?window=N`).
- Status select + free-text search.
- `Refresh` button + `auto (5s)` checkbox + `live feed` checkbox (SSE-fed task events).
- Tasks list (paginated, cursor-driven, virtual-scroll-ish).
- **Task detail panel** (right-hand split):
  - Investigation banner (when set)
  - W2-001e/f Replay banner with duration-delta vs original (when this task is a replay)
  - Summary row
  - Retry chain pills (attempts with arrows + inter-attempt waits)
  - SVG exec graph with critical-segment highlight
  - Failure panel
  - Topology-correlation slot (async-filled for failed/interrupted)
  - Execution-path slot (async-filled when chronicle has `capability.invoked`)
  - Lineage panel slot (async-filled by `/v1/tasks/:id/lineage_graph?depth=4`)
  - Attempts table
  - Todo widget (async-filled)
  - **Chronicle / Timeline** with:
    - W2-003c category chips (`all / capability / attempt / error / retry / pause / lifecycle`) with per-category counts (W2-003d)
    - W2-003c text filter (event_type + payload substring)
    - W2-003e URL persistence (`?cat=`, `?chq=`)
    - Per-step duration (`+Xs since prev`, W2-001a)
    - W2-002h inline failure-screenshot thumbnails (when payload contains `screenshot=<path>`)
    - Timeline / Raw toggle
  - Cross-references panel (task_id, trace_id, flow_id, flow_log_path) + CLI cheat sheet
- Detail action bar: Export, Replay (W2-001d), Retry, Cancel, Investigate, Note, etc.

### 4.3 `#/topology`

- Peers list with alias, node_type, freshness (`fresh / stale / expired`), reconnect counters.
- Peer detail drawer with manifest preview.
- Lifecycle event ring (joins / freshness changes / drops, 500 entries).
- Routing snapshot (per-capability → peer routing decisions, `/v1/routing`).

### 4.4 `#/capabilities`

- Filter chips by category (`all / task / tool / browser / mcp / memory / ai / node`).
- Text filter (capability name substring).
- Full method list with alias, node_type, freshness, last-refresh.
- **W2-007c Policy "What If?"** form — peer / method / groups → decision badge + matched rule + reason.
- **W2-007f Recent denials** card — peer / max controls → table of recent policy denies.

### 4.5 `#/mcp`

- Peer alias input + refresh.
- Registered servers table (declared + tool count).
- Click `expand tools` per row → fetch `tool.mcp.list_tools`.
- **Recent invocations** ring (bounded 256, newest first).

### 4.6 `#/fsaudit`

- Peer / op (`write / append / patch / fuzzy_replace`) / max controls.
- Recent mutations table (per-jail ring).
- **Web host blocklist** card — operator-curated `[tool] blocked_hosts` summary.

### 4.7 `#/termaudit`

- Peer / max controls.
- Completion ring of `tool.terminal.run` / `spawn` invocations.

### 4.8 `#/browser`

- Active session inspector (peer alias → sessions list with current URL + status).

### 4.9 `#/metrics`

- Peer alias input + `refresh` button + **W2-008e auto-refresh dropdown** (off / 5s / 15s / 60s).
- Per-capability table sorted by mean latency desc: method / invocations / errs+denied / mean / max / last / samples / **trend** (W2-006d inline SVG sparkline of last 32 latencies, normalized to row's own max).

### 4.10 `#/providers`

- Consolidated AI provider health: per-provider configured, rate-limit hits 5m/1h, last failure, cooldown active, quarantined.
- Aggregate counters: success / fail / reliability%.
- Route-test card (HealthAwareRouter preview).

### 4.11 `#/telegram`

- Bot token status, webhook config, allowed user groups, recent-message ring. ~~**Scaffold UI** — the live HTTPS client is not implemented.~~ **[SHIPPED in 0.4.1 — live HTTPS client, long-poll + webhook mode, approval notifier, voice transcription. See §6.1.]**

### 4.12 `#/config`

- Effective bridge config (read-only).
- Provider key cards (set / clear / preview) — secrets written to `bridge-secrets.toml`.
- Telegram settings (token, delivery mode, allowed users).

### 4.13 Toast / modal system

Global toast host (warn / error / ok). Retention modal at the bottom of
the tasks page (chronicle-compaction dry-run UI).

### 4.14 Keyboard shortcuts

`j` / `k` navigate task list. `/` focuses search. `?` opens help. `1`–`9`, `0` switch routes. `[` / `]` reserved (not wired today).

---

## 5. WHAT THE CLI CAN DO

`relix-cli` is the developer + operator CLI. 15 top-level subcommands.
Source: `crates/relix-cli/src/main.rs`.

### 5.1 Identity ceremony — libp2p calls, no bridge

| Command | What it does |
| --- | --- |
| `identity init-org` | Mint an org root keypair |
| `identity mint --name --groups --out` | Mint an AIC (Agent Identity Credential) signed by the org root |
| `identity show <bundle>` | Decode and print bundle contents |
| `identity verify <bundle> --root-key` | Verify signature against org root |

### 5.2 Direct libp2p — dial a peer

`ping --peer <multiaddr> --identity <aic> --method <name> --client-key <key>` — the lowest-level diagnostic.

### 5.3 Task ledger — libp2p to coord

`task create / update / get / list / events / attempts / recent-events / retry / replay / recover / note / mark-investigation / export / compact-events / count`. Each subcommand dials the coordinator peer.

### 5.4 Capability inspection

`capability list --peer <multiaddr>` — fetch and print a peer's manifest. `capability validate <descriptor>` — local manifest validator.

### 5.5 Topology — HTTP to bridge

`topology` — bridge's `/v1/topology` snapshot pretty-printed.

### 5.6 Operations — HTTP to bridge (the big one)

`relix-cli ops` has the most subcommands:

| Subcommand | What it does |
| --- | --- |
| `providers-health` | `/v1/providers/health` pretty print |
| `capabilities` | `/v1/topology` aggregated as method list |
| `stuck` | `/v1/tasks/stuck?threshold_secs=N` |
| `events` | `/v1/tasks/events/recent` with `--filter`, `--json`, `--csv` (W2-008f) |
| `route-test` | `POST /v1/providers/route_test` |
| `dispatch-stats` | `/v1/dispatch/stats?peer=X` with Unicode sparkline column (W2-006e) |
| `policy-simulate` | `/v1/policy/simulate` (W2-007g) |
| `policy-denials` | `/v1/policy/denials` (W2-007g) |
| `smoke` | 5-step end-to-end mesh smoke (W2-008c) |
| `tail` | Live firehose tail via `since=` cursor polling (W2-008d) |
| `openwebui-setup` | Print copy-paste Open WebUI config from `/v1/models` (W2-008h) |
| `snapshot` | One-shot JSON dump of every observable bridge state (W2-008i) |

### 5.7 Router — libp2p to router

`router heartbeat / summary / sessions / log`. Same dial-and-call pattern as `ping`.

### 5.8 MCP — libp2p to tool

`mcp servers --peer X` lists registered MCP servers. `mcp tools --peer X --server <id>` lists declared tools.

### 5.9 Fs / Web / Browser / Terminal mirrors — HTTP to bridge

- `fs audit` → `/v1/fs/audit`
- `web blocklist` → `/v1/tool/blocklist`
- `browser sessions` → `/v1/browser/sessions`
- `terminal sessions / audit / cancel`

### 5.10 SOL authoring

`sol templates` lists baked-in workflow templates (`include_str!`-ed at compile time). `sol new --template ping --out my.sol` writes one to disk (W2-004a).

### 5.11 Doctor

`doctor` — hits `/v1/health` + checks env, prints opinionated PASS / WARN / FAIL, exit nonzero on any FAIL (W2-008a).

### 5.12 Flow run — libp2p to mesh

`flow-run --flow <path> --identity <aic> --client-key <key> --peers <toml>` — compiles a `.sol`, dials every named peer, runs the VM, prints result + flow log path.

### 5.13 Other binaries

- `relix-flow-inspect --flow <path>` — read per-flow event log; `--replay-verify` walks the hash chain + verifies signatures.
- `relix-flow-inspect --audit <path>` — read per-responder audit log with `--trace`, `--rid` filters.

---

## 6. WHAT IS PARTIALLY BUILT

Things that exist in the source tree but are not complete. Cited
honestly — these are real code that runs, but ships with a known gap.

### 6.1 Telegram channel — shipped

`relix-telegram` now ships a live `BotApi` HTTPS client
(reqwest + rustls, no openssl) covering getMe / getUpdates /
sendMessage / answerCallbackQuery / editMessageText / sendChatAction
with 429 + 5xx retry semantics. `node_type = "telegram"` is wired
into the controller binary; it long-polls for inbound messages,
enforces `allowed_users`, handles `/start /help /status /memory
/forget /approve /reject`, runs the equivalent of `flows/chat_template.sol`
(`memory.recent_for_session` → `ai.chat` → `memory.write_turn`), creates
a coordinator task per turn with `origin_surface = "telegram"`, and
optionally posts approval-required notifications to a configured
operator chat. Bridge endpoints `GET /v1/telegram/status` and
`GET /v1/telegram/messages/recent` proxy the channel's read
capabilities; the dashboard `#/telegram` page renders both as live
cards. Setup is documented in `docs/channels/telegram.md`. Boot via
`scripts/relix-mesh-up.ps1` with `$env:RELIX_TELEGRAM = "1"` and
`$env:RELIX_TELEGRAM_BOT_TOKEN = "<token>"`.

### 6.1b Discord channel — shipped

`relix-discord` ships a live `DiscordApi` HTTPS client (reqwest +
rustls) covering `getMe` / `getMessages` / `sendMessage` /
`sendTyping` / `deleteMessage` with 429 (`retry_after` is a
float, clamped 1..30s) and 5xx retry semantics. `node_type =
"discord"` is wired into the controller binary; it polls
`GET /channels/:channel_id/messages?after=:last_id` on a
configurable interval (default 2s), filters bot self-loops by
matching the cached `get_me` user_id and `author.bot=true`,
enforces `allowed_users`, handles `/help /status /memory
/forget`, runs the same memory + ai dispatch sequence Telegram
uses, and creates a coordinator task per turn with
`origin_surface = "discord"`. Snowflake ids (user_id, channel_id,
message_id) are strings end-to-end — they exceed JS safe-int
range. Bridge endpoints `GET /v1/discord/status` +
`GET /v1/discord/messages/recent` proxy the channel's read
capabilities; the dashboard `#/discord` page renders both as
live cards; `relix-cli ops discord {status,messages}` mirrors the
same data on the terminal. Boot via
`scripts/relix-mesh-up.ps1` with
`$env:RELIX_DISCORD = "1"`,
`$env:RELIX_DISCORD_BOT_TOKEN = "<token>"`, and
`$env:RELIX_DISCORD_CHANNEL_ID = "<channel-snowflake>"`.
Setup is documented in `docs/channels/discord.md`.

The Discord controller deliberately ships smaller than Telegram:
no Gateway/WebSocket client (REST polling only), no webhook
delivery mode, no approval-notifier loop, no persistent session
store, no formal slash-command registration. `operator_user_id`
is reserved for the future approval surface.

### 6.1c Slack channel — shipped

`relix-slack` ships a live `SlackApi` HTTPS client (reqwest +
rustls) covering `auth.test` / `conversations.history` /
`chat.postMessage` / `chat.update`. **HTTP 200 + ok=false** is
the Slack error model — the generic request helper parses the
envelope first and maps `ok: false` to `ClientError`, never
retried. 429 honours the `Retry-After` HEADER (integer seconds,
clamped 1..30s); 5xx uses exponential backoff (1s, 2s, 4s, max
3 retries).

`node_type = "slack"` is wired into the controller binary; it
polls `POST /api/conversations.history` on a configurable
interval (default 2s) using the per-message `ts` string as the
`oldest` cursor, filters bot self-loops at two layers (SDK
parse drops `subtype` + `bot_id` messages; controller adds a
`user_id == bot.user_id` check), enforces `allowed_users`,
handles `/help /status /memory /forget`, runs the same
memory + ai dispatch sequence Telegram + Discord use, and
creates a coordinator task per turn with
`origin_surface = "slack"`. Replies are threaded under the
inbound message's `ts`. There is **no typing indicator** —
Slack has no REST equivalent and the implementation does not
invent one.

Snowflake-ish ids (user_id, team_id, bot_id, channel_id) are
strings end-to-end. The status capability exposes a `team_id`
slot in addition to user_id / username — Slack's identity model
has a workspace dimension that Discord and Telegram don't.

Bridge endpoints `GET /v1/slack/status` +
`GET /v1/slack/messages/recent` proxy the channel's read
capabilities; the dashboard `#/slack` page renders both as live
cards; `relix-cli ops slack {status,messages}` mirrors the same
data on the terminal. Boot via `scripts/relix-mesh-up.ps1` with
`$env:RELIX_SLACK = "1"`,
`$env:RELIX_SLACK_BOT_TOKEN = "xoxb-..."`, and
`$env:RELIX_SLACK_CHANNEL_ID = "C01234567"`. Setup is documented
in `docs/channels/slack.md`.

The Slack controller deliberately ships smaller than Telegram:
no Socket Mode WebSocket, no Events API webhook receiver, no
typing indicator, no approval-notifier loop, no persistent
session store, no formal slash-command registration.

### 6.1d Plugin system — shipped

Third-party plugins extend Relix without modifying the core
codebase. A new `node_type = "plugin_host"` controller scans a
`plugin_dir` for `plugin.toml` manifests, spawns each plugin as
a subprocess, reads `RELIX_PLUGIN_PORT=<n>` from stdout
(10s timeout), polls `/health` until 200 (30s timeout), then
registers each declared capability on its dispatch bridge as a
`FnHandler` that routes calls to `POST /invoke` on the plugin.

The protocol is `relix-plugin-v1` — HTTP/JSON over localhost,
documented in `docs/plugins.md`. Plugin authors get a tiny
Rust SDK (`relix-plugin-sdk` crate, axum-based, no relix-runtime
dep) so the boilerplate is one `PluginServer::new()` +
`register()` + `serve()`. Python and any other language with
an HTTP server can implement the protocol directly.

The plugin_host node registers four management capabilities:
`plugin.list / status / reload / disable`. A SQLite registry
(`plugin-registry.db`) persists `(plugin_id, name, version,
status, error_message, registered_at, last_seen_at,
capabilities JSON)` across reboots. Status moves
`registered → active → error → disabled`.

Bridge endpoints `GET /v1/plugins`, `GET /v1/plugins/:id`,
`POST /v1/plugins/:id/{reload,disable}` proxy the management
caps; the dashboard `#/plugins` page lists plugins and lets
operators reload / disable from a detail card;
`relix-cli ops plugin {list,status,reload,disable}` mirrors
the same on the terminal.

Two worked examples ship in `examples/plugins/`:

- `hello-plugin/` — Python stdlib only. `hello.greet` capability.
  Smallest possible implementation of the protocol.
- `web-lookup/` — Rust + `relix-plugin-sdk`. `web_lookup.fetch`
  capability. Validates http(s) scheme + caps response body at
  500 chars + maps reqwest errors to typed `PluginError` kinds.

Boot via `scripts/relix-mesh-up.ps1` with
`$env:RELIX_PLUGINS = "1"`; optional
`$env:RELIX_PLUGIN_DIR = "./examples/plugins"`. Policy rules
for the management caps + the example plugins' methods are
written automatically.

Subprocess isolation is the security boundary: a panicking
plugin can't take the host down, the host kills the children
on drop (tokio `Command::kill_on_drop(true)`), and plugin
methods pass through the same `PolicyEngine` admission as
built-in capabilities so operators control access in the same
TOML they already use.

Deliberate non-goals (today): no WASM runtime, no
automatic plugin restart on crash (use `plugin.reload`), no
remote plugin discovery (plugins must be on the same host as
the plugin_host node), no streaming response shape (one-shot
JSON request/reply only). The protocol leaves room for these
without breaking compatibility.

### 6.2 Playwright browser backend — shipped

`[tool.browser] backend = "playwright"` selects the live driver
(behind the `browser-playwright` Cargo feature). It lazy-spawns a
Node.js sidecar that imports `playwright-core` and speaks
newline-delimited JSON-RPC over stdio. Six trait methods are
real today: `open_session`, `close_session`, `navigate`,
`get_text`, `screenshot`, `list_sessions`, plus `click`,
`type_text`, and `wait_for_selector` (F12). Operators without
Node + playwright-core installed get a precise
`BackendNotConnected` cause from the sidecar's startup error
envelope — no fake success. The default backend is still
`headless_chrome`; switching is a config-only change.

### 6.3 MCP HTTP transport — shipped

`tool.mcp.invoke` over HTTP transport runs a live JSON-RPC POST
against the configured endpoint (F13). The client speaks the
Streamable-HTTP variant of MCP — one POST per request, JSON
response. Auth header is forwarded when the server config
sets `auth_header`. Transient failures (connect / 5xx / 429)
retry with exponential backoff up to `reconnect_max` attempts
(default 3). Boot-time discovery runs `tools/list` against
every HTTP server and logs the discovered tool count; a server
down at boot doesn't fail the tool node. The legacy HTTP+SSE
variant (initialize once, subscribe to SSE for streamed
responses) is NOT supported — operators who need that should
continue with stdio for now.

### 6.4 OpenAI shim drops fields

`POST /v1/chat/completions` accepts the full request shape but silently
ignores: `temperature`, `top_p`, `n`, `presence_penalty`,
`frequency_penalty`, `logit_bias`, `tools`, `tool_choice`,
`response_format`, `seed`, `stop`, `stream_options`. The bridge sends
only `model` + `messages` to the AI node. SIMP-020.

### 6.5 Streaming — end-to-end real (opt-in)

End-to-end token streaming is real and shipped, opt-in by a
single bridge config line. Tokens flow from the AI provider's
SSE response, through the mesh's libp2p streaming substream,
through the SOL VM's `remote_call_stream` opcode, through a
chunk-observer callback, into the bridge's SSE response, out
to the HTTP client — all live, with no intermediate
materialisation. The same admission pipeline runs (identity →
agent gate → policy → access broker → audit) and the same
audit / chronicle / task-ledger events fire as the unary
path.

Architecture (commits `4b58550` → `26a8660`):

  * **Transport:** `transport::stream` adds a real libp2p
    substream protocol `/relix/rpc/stream/1` with a
    `StreamFrame` enum (Header / Chunk / End / Err) over
    length-prefixed CBOR framing. Yamux multiplexing means
    one TCP connection multiplexes both `/relix/rpc/1`
    (unary) and `/relix/rpc/stream/1` (streaming) over the
    same Noise XK session.

  * **Dispatch:** `DispatchBridge::handle_inbound_stream` +
    `StreamingHandler` trait + `register_streaming(method,
    handler)`. Mirrors the unary admission flow
    step-for-step but routes the response through a
    `StreamWriter` instead of a unary
    `ResponseEnvelope`. Admission rejection writes a single
    terminal `StreamFrame::Err`; admission success writes a
    Header frame, invokes the handler, pipes its chunks to
    Chunk frames, terminates with End or Err.

  * **AI node:** `ai.chat.stream` capability registered via
    `bridge.register_streaming`. Shares the FULL pre-flight
    with `ai.chat` (input guardrail, memory + RAG, SOUL
    persona, skill hints) — extracted into a shared
    `build_chat_preflight` helper so the two paths can
    never drift. Differs by calling
    `provider.generate_reply_stream` and adapting the
    per-token stream to the dispatcher's `HandlerStream`.
    The streaming variant intentionally SKIPS the planner /
    tool dispatch / approval verdict pipeline — operators
    needing inline tool execution use the unary `ai.chat`.

  * **SOL VM:** `Inst::RemoteCallStream` opcode +
    `remote_call_stream(peer, method, arg)` parser
    recognition. Same stack contract as `Inst::RemoteCall`;
    the VM still produces a single concatenated heap-string
    so SOL flows stay synchronous from the author's
    perspective. The chunk observer fires per-chunk so the
    web bridge can ship tokens to the HTTP client BEFORE
    the VM has finished collecting.

  * **Flow runner:** `FlowRunOptions.chunk_observer` +
    `FlowRunOptions.cancel_signal`. The
    `ChunkObserver` callback type is wired into
    `VM::with_chunk_observer`; the cancel signal is wired
    into `RealDispatcher::remote_call_stream`'s
    frame-read `tokio::select!` so an in-flight stream
    aborts cleanly when the bridge cancels.

  * **Bridge:** `[flow] streaming_template_path` config
    field (validated at startup — the template MUST invoke
    `remote_call_stream` or the bridge refuses to boot).
    `flows/chat_template_streaming.sol` ships alongside
    the unary template. When `stream:true` AND the
    streaming template is configured,
    `chat_completions_streaming` runs the flow in a tokio
    task, the SOL VM's chunks land in an unbounded channel,
    and the SSE response reads from the channel via
    `async_stream::stream!`. A `CancelGuard` inside the
    SSE future fires `notify_one` on the cancel signal
    when the HTTP client drops — the dispatcher aborts,
    the flow writes `task.failed` audit events, no
    orphaned upstream connection.

  * **Wire shape pins:** `streaming_role_chunk_json` /
    `streaming_content_chunk_json` /
    `streaming_finish_chunk_json` are pure functions with
    6 wire-shape regression tests (`openai::tests::streaming_*`).
    The `[DONE]` sentinel is a `pub(crate) const` so
    drift breaks tests immediately.

Operators enable end-to-end streaming with one config line:

```toml
[flow]
template_path = "flows/chat_template.sol"
streaming_template_path = "flows/chat_template_streaming.sol"
```

When `streaming_template_path` is unset, `stream:true` falls
back to the legacy chunk-sliced path so existing deployments
behave byte-identically. The non-streaming `stream:false`
path is unchanged regardless of config.

Test coverage:

  * Transport: 8 integration tests
    (`tests/transport_stream.rs`) including multi-chunk
    round-trip, caller-drop cancellation, admission +
    streaming end-to-end (5 admission paths), real-
    dispatcher cancel-signal-honours mid-stream.
  * AI handler: 4 unit tests for `handle_chat_stream`
    (happy path, invalid args, guardrail block, provider
    init failure).
  * VM: 5 tests for `Inst::RemoteCallStream` (concatenation,
    per-chunk observer firing, default-impl fallback,
    failure → sentinel) + 1 compile-pipeline test.
  * Bridge: 6 wire-shape unit tests for the SSE chunk
    builders (role / content / finish / DONE sentinel /
    full sequence + task_id-null when no coordinator).
  * Bridge: 1 mini-mesh integration test
    (`streaming_mini_mesh_test`) that boots a real libp2p
    AI peer, builds a full `AppState` via `try_new` from
    on-disk config (identity bundle + peers.toml + SOL
    streaming template), stands up an axum router with
    `POST /v1/chat/completions`, and sends a real
    `stream:true` HTTP request — asserting role marker,
    ordered content chunks reassemble to the handler's
    output, `finish_reason: stop`, the Relix metadata
    envelope, and the literal `[DONE]` sentinel. Catches
    plumbing regressions (config, dial, identity bundle
    decode, admission against `org_root`'s verifying key,
    SOL template rendering, FlowRunner per-request
    ephemeral peer dial) that wire-shape unit tests can't
    see. Coordinator is intentionally absent — the test
    pins the streaming path, not task persistence.

This closes SIMP-019.

### 6.6 Manifests are signed (shipped as of 0.4.1) ~~[was: gap]~~

`NodeManifest` is now sent inside a `SignedManifest` envelope (Ed25519,
TOFU pinning) as of RELIX-5 PART 2. `ManifestProvider::signed_snapshot`
signs `CBOR(NodeManifest)` with the node's own key; `discover_and_pin`
verifies the signature and cross-checks the signer pubkey against the
Noise-authenticated PeerId. The bridge no longer blindly trusts
self-reported capability lists. ~~SIMP-006 was the original tracking
entry; the alpha signing mechanism differs from the full
`BundleType::NodeManifest` bundle chain targeted for Gate 2 — that
upgrade is still future, but basic manifest integrity is real today.~~

### 6.7 Identity bundles have one delegation level

Org root signs IdentityBundles directly. No Intermediate Authority
layer. Compromised org-root = compromised mesh. SIMP-002.

### 6.8 No revocation gossip

Default bundle lifetime from `relix-cli identity mint` is 24h. The
only way to invalidate is to wait for expiry. SIMP-003.

### 6.9 No DHT-based discovery

`bootstrap_kademlia` is called at startup but there is no working DHT
peer-find or capability gossip. Peer addresses come from the static
`peers.toml`. SIMP-007 / -017.

### 6.10 `tool.web_fetch` is GET-only and text-only

POST / PUT / DELETE are not exposed (separate `tool.web.post` exists
but with its own restrictions). Response bodies must decode as UTF-8
and have a text-ish content type.

### 6.11 Tool node pool has no LRU eviction

`PinnedClientPool` grows one entry per unique `(hostname,
validated_addrs)`. Soft cap of 256 emits a WARN; eviction lands later.

### 6.12 No per-`remote_call` task events from the bridge

The bridge writes `task.created` / `flow.started` /
`task.completed|failed` and a single `capability.invoked` on the tool
path. Per-call detail lives in the per-flow event log on disk
(reachable via `task.latest_flow_log_path`), not in the chronicle.

### 6.13 Bridge does not use the `running` task state

`pending` → `completed|failed` directly. Operators driving tasks
manually via `task update --status running` use it; the canonical
bridge path skips it.

### 6.14 No rate limiting

The policy engine is allow / deny only. Cost-class-aware throttling
(the `CapabilityDescriptor::cost_class` field exists for it) is not
implemented.

### 6.15 No audit aggregator

Each controller maintains its own hash-chained audit log
(`dev-data/<run>-<node>/audit.log`). Cross-node correlation is by
`request_id` / `trace_id` shared in both logs. Operators are expected
to ship logs to a SIEM.

### 6.16 No standalone log rotation

`dev-data/<run>/{memory,ai,tool,bridge}.log` grow unbounded. Audit
logs are the integrity-relevant ones.

### 6.17 Cross-host redirect window in tool node

The SSRF guard re-runs on every redirect hop, but reqwest re-resolves
DNS after the guard validates — sub-millisecond rebinding window. For
zero-window posture, set `[tool] max_redirects = 0`. Documented in
`docs/tool-node-security.md`.

### 6.18 Provider `local` (Ollama / llama.cpp / vLLM) is not stress-tested

Works for deterministic prompts. Failure modes (model not loaded,
context overflow, GPU OOM) surface as generic provider errors with
no graceful fallback.

### 6.19 Static peer alias map is still load-bearing

Even with capability discovery, every peer the bridge talks to must
be in `peers.toml`. `capability:<method>` routing chooses between
aliases in that file; it does not discover new peers.

### 6.20 No replay timeline UX

Replay creates a new task with `retried_from` edge + `task.replayed_from`
chronicle event (W2-001b). Dashboard shows a banner with duration-delta
vs the original (W2-001e/f). **There is no side-by-side comparison
view, no event-level diff, no replay-with-overrides, no dry-run mode.**
A full design proposal was started in this session and explicitly
paused; see §7.

---

## 7. WHAT IS PLANNED BUT NOT STARTED (0.4.1 update)

Anything that has a docs / proposal / decision-pending entry but no
implementation. Items from the original list that have since shipped are
annotated.

### 7.1 Proposals

| Proposal | Status |
| --- | --- |
| `docs/proposals/agent-employee-permissions.md` | **Shipped end-to-end** in 0.4.1 (see §8). |

### 7.2 Replay UX V2

Still not started. Three slices were proposed:
- **Slice A** — lineage breadcrumb + side-by-side chronicle comparison
- **Slice B** — event-level diff chips + screenshot side-by-side + outcome-coloured retry pills
- **Slice C** — replay endpoint extensions (`overrides`, `mode: execute|dryrun`)

This track was explicitly paused. The replay primitive (`task.replay`,
dashboard duration-delta banner) exists; the UX comparison surface does not.

### 7.3 Decisions-pending entries (`docs/internal/decisions-pending.md`)

Prior state: D-001 through D-007 marked `defer`, D-008 through D-010 marked
`shipped`. Open / future: token throttling, MCP HTTP+SSE legacy transport,
multi-org boundaries — not actively in a slice.

### 7.4 Other "docs without code" items (annotated)

- `docs/channel-node-architecture.md` — scaffold scaffolding; **stale** — Telegram, Discord, Slack, Email are all shipped. The "NOT in scope" bullets (webhook, voice, multiple channels) are now shipped.
- `docs/replay-model.md` — still accurate: SOL VM is synchronous, pause-and-resume is still hard.
- `docs/plugin-foundations.md` — **partially stale**: the subprocess plugin system ships as of 0.4.1 (§6.1d). The M1/M2/M3 constraints remain correct; WASM sandbox and marketplace are still future.
- `docs/multi-node-bringup.md` — describes `relix-mesh-up.ps1` flow; still accurate.
- `docs/dashboard-redesign.md`: the operator dashboard rebuild shipped (May 31, 2026) as 18 sections, later extended to 22 by RELA-31 (Tasks, Scheduled Jobs, Policy Denials, MCP Servers). Some redesign refs are still useful for context.
- `docs/production-checklist.md` — operational gates for a hypothetical production deploy. Not a track.

### 7.5 Specs / SIMP entries deferred to Gate 2

Mentioned throughout `docs/current-limitations.md` and in source comments:
- SIMP-002 — Intermediate Authority layer for identity bundles
- SIMP-003 — CRL / revocation gossip
- ~~SIMP-006 — manifest signing~~ **[PARTIALLY SHIPPED in 0.4.1 — Ed25519 SignedManifest + TOFU; full BundleType::NodeManifest chain deferred to Gate 2]**
- SIMP-007 / -017 — DHT / capability gossip
- SIMP-016 — typed CDDL replaces `String`-shaped args at SOL boundaries
- SIMP-018 — typed flow arguments (replaces character-level template substitution)
- ~~SIMP-019 — provider-native streaming~~ **[SHIPPED in 0.4.1; see §6.5]**
- SIMP-020 — OpenAI shim field coverage

---

## 8. THE AGENT EMPLOYEE PERMISSION MODEL

**Status: shipped end-to-end.** All five phases from the
proposal land. The operator-facing handbook is
[`agent-employee-model.md`](agent-employee-model.md); the
deep-dive design + per-deny-reason vocabulary is
[`agent-permissions.md`](agent-permissions.md); the original
proposal stays at
[`proposals/agent-employee-permissions.md`](proposals/agent-employee-permissions.md).

What's actually in the codebase today:

- **Agent profile store** — SQLite-backed
  `agent_profiles` / `approval_requests` /
  `standing_approvals` tables in
  `crates/relix-runtime/src/nodes/coordinator/agent/store.rs`,
  with full CRUD + token / standing lifecycle.
- **Coordinator capabilities** — `agent.create / get / list /
  update / delete`, `coord.approval.pending / decide`,
  `agent.standing_approval.list / create / revoke` registered
  in `nodes/coordinator/agent/handlers.rs`.
- **Bridge HTTP routes** — `/v1/agents`, `/v1/agents/:id`,
  `/v1/approvals`, `/v1/approvals/:id/decide`,
  `/v1/agents/:id/standing-approvals`,
  `/v1/standing-approvals/:id` in
  `crates/relix-web-bridge/src/agent.rs`. Routes registered in
  `main.rs:587–612`.
- **Dispatch-pipeline integration** — `admission::agent_gate`
  evaluates between identity validation and policy
  (`dispatch/mod.rs:523`). The bridge's `AgentGateBindings`
  carries the `AgentStoreHandle` + `on_require_approval`
  closure that materialises pending rows when the gate
  pauses a task.
- **Envelope surface + approval_token fields** —
  `RequestEnvelope.surface` and
  `RequestEnvelope.approval_token` ride on every call
  (`transport/envelope.rs:40–49`). Both are additive +
  default `None` for backward compat.
- **Telegram approval bridge** — `/approve <id>` and
  `/reject <id> <reason>` slash commands in the operator
  chat call the coord approval-decide bridge.
  `operator_chat_id = 0` disables the notifier.
- **Dashboard pages** — `#/agents` (create form + list +
  detail + edit) and `#/approvals` (pending queue, decide
  inline) live in `dashboard.html` under their
  `<section data-page="agents">` /
  `<section data-page="approvals">` blocks.
- **Tests** — `admission/agent_gate.rs` covers status,
  surface, risk ceiling, categorical allow / deny, standing
  approval, approval token, expired token, and consumed
  token paths. `nodes/telegram/controller.rs` covers
  `/approve` and `/reject` operator-only gating + the
  coord bridge dispatch path.

Honest limitations on top of the shipped surface:

- The `surface` field is operator-asserted — a compromised
  bridge could fake it. The gate raises the cost of misuse
  ("spoof the bundle AND the surface tag") without
  pretending to be a cryptographic boundary. SIMP-002 +
  surface-signing land in a later wave.
- Approval expiry is a per-agent
  `approval_timeout_secs` (default 86400). Multi-approver
  (2-of-3) workflows are not in the alpha; single-approver
  with reason field is what ships.
- The dashboard does not push real-time approval
  notifications (no SSE); operators poll the list page or
  read the Telegram notifier output.

---

## 9. MEMORY

### 9.1 What exists

A single `memory` node type backed by **SQLite + FTS5**.

**Per-turn memory** (chat history):
- `memory.write_turn` — persist a turn (session_id, role, body)
- `memory.recent_for_session` — read last N turns oldest-first (default 10)
- `memory.search` — full-text search via FTS5 across all turns

**Persistent agent memory** (W2-MEMORY, frozen-snapshot pattern,
patterned on Hermes's `MEMORY.md` + `USER.md`):
- `memory.agent_read` — read agent + user memory for a `subject_id`
- `memory.agent_write` — add / replace / remove / read one target

Two text stores per agent (keyed by the agent's `subject_id`):
- `agent` target — agent's notes about environment, tools,
  project conventions, facts. Char cap 2200.
- `user` target — what the agent knows about the user it
  serves — preferences, communication style, workflow habits.
  Char cap 1375.

Entries within a target are separated by `§` (U+00A7). Char
caps enforced on every write; INVALID_ARGS on overflow.

Storage path: `[memory] db_path` in the controller config, typically
`dev-data/<run>/memory.db`. SQLite is the only backing store. The
W2-MEMORY work adds an `agent_memory` table alongside the
existing `turns` table.

### 9.2 How chat flows use memory

**Per-turn** (in `flows/chat.sol` and `flows/chat_with_tool.sol`):
1. Persist user turn first (so recent-history readback includes it).
2. Read recent history.
3. Pass `session_id | prompt | history` to `ai.chat`.
4. Persist assistant turn.

The order is SOL-encoded; the runtime does not enforce it.

**Persistent (frozen-snapshot)**: when the AI controller is
configured with `[ai.memory_peer]`, the AI node's `ai.chat`
handler reads `memory.agent_read` ONCE per chat call and
prepends a labelled `--- AGENT MEMORY ---` / `--- USER MEMORY
---` block to `ChatInput.system_prompt` before invoking the
provider. Mid-session memory writes go to disk immediately but
the running session's prompt does NOT re-render — the snapshot
refreshes on the next session. Silent skip on any failure.

Operators inspect persistent memory via:
- Dashboard `#/memory` page (read-only)
- `relix-cli ops agent-memory --subject-id <hex>` (read-only)

Full doc: [`agent-memory.md`](agent-memory.md).

### 9.3 Status as of 0.4.1 (shipped and remaining gaps)

**Shipped since the original audit:**

- **Vector embeddings (Qdrant + Layer 2/3/4 store).** The four-layer
  `LayeredMemoryStore` ships with per-tenant Qdrant collections, a
  background embedding pipeline, layer promoter, consolidation archiver,
  anomaly scorer, quarantine flow, and integrity auditor. FTS5 search
  remains on the legacy `MemoryStore`; semantic search via `memory.search`
  and `memory.records_search` targets the vector layers.
- **Cross-agent knowledge sharing.** Eight `knowledge.*` capabilities
  (share, broadcast, recall, accept_shared, autoshare_stats, signed
  payloads, group resolver, auto-share background task). Per-group
  policies control which layers propagate automatically.
- **Background curator.** `ConsolidationArchiver` (6h interval),
  `MemoryIntegrityAuditor` (24h), and the layer promoter loop are all
  running background tasks. The curator scheduler (`memory.curator_status`)
  is also real.
- **Write-time PII validation.** `PiiAnonymizer` runs at record-time;
  `BulkAnonymizeRecords` can retroactively clean a store.
- **Bi-temporal validity.** `valid_from` / `valid_to` / `superseded_by`
  columns with `supersede()`, `as_of()`, and `supersedes_chain()` helpers.

**Remaining honest gaps:**

- **Per-session scope** on persistent memory — the `scope` column +
  vocabulary decision is deferred; a future commit will add it.
- **Hard-delete cascade** — the inspector uses `invalidate` (soft), not
  physical row deletion + Qdrant point removal.
- **Memory time-bounding on `recent_for_session`** — the FTS5 layer
  still treats all sessions equally by recency; no date-window filter.
- **Separate low-priority Qdrant archive segment** — infeasible in the
  current single-collection deployment without a breaking schema change.

---

## 10. SOL AND POLICY

### 10.1 SOL today

Relix ships **two** flow languages side-by-side. The `flow_runner`
dispatches on file extension: `.sol` → SOL VM, `.sflow` → Sflow
AST executor. Both run against the same `RemoteCallDispatcher`
and write to the same per-flow event log.

**SOL (`.sol`)** — the Rust-like language ported from OpenPrem. A
real little programming language: typed `let` bindings, `{}`
blocks, `;`-terminated statements, function definitions,
`if`/`else`, `while`, `for-in`, structs, enums, arrays. The VM is
a stack machine with `Jump`/`JumpFalse` opcodes; codegen lowers
control flow to those instructions. The mesh primitive is
`remote_call(peer_alias_or_capability_uri, method, args) -> str`.
String concatenation uses `+`; literals are quoted with no escape
sequences (SIMP-016).

**Sflow (`.sflow`)** — the step-based DSL added in W4. Flat
sequence of statements, no functions, no types, no semicolons.
Named steps via `step <name>: <peer>.<method> "arg"`, variables
via `set x = ...`, `${var}` interpolation in step args, `if /
elif / else / end`, `loop N times / end`, `while / until / end`,
`try / catch <kind> / rethrow / end`, `sol.log /sleep /assert
/set_result` built-ins. Error kinds map to `RemoteCallError`
(`timeout`, `mesh_error`, `policy_denied`, `responder_error`,
`any`). Hard caps: 50 vars per execution, 100 loop iterations
(configurable), 8-deep nesting.

Argument convention: still pipe-delimited strings
(`session|prompt|history`). SIMP-016 keeps the
SOL ↔ handler boundary as `String` to avoid inventing a SOL
type system. Gate 2 replaces this with typed CDDL.

Bridge templates substitute `{{SESSION}}`, `{{MESSAGE}}`,
`{{TOOL_URL}}` into `.sol` files before compiling. The
substitution validator rejects `"`, `|`, and `\n` in user input.
SIMP-018. The same validator applies to `.sflow` templates when
the bridge starts rendering them.

`POST /v1/sol/validate { source, kind }` parses either language
without executing it and returns line-numbered errors —
dashboard editors call it on demand. See `docs/sol.md` for the
full language reference.

### 10.2 What SOL / Sflow cannot do

- **List & map literals shipped (both languages).** SOL has
  `Type::List` / `Type::Map`, `[a, b, c]` and `{ "k": v }`
  literal syntax, twelve `list_*` / `map_*` built-ins, and
  `for x in lst { … }` iteration. Sflow has the same surface
  with values stored as a typed `SflowValue` enum; values
  stringify as `a|b|c` (lists) and `k1=v1;k2=v2` (maps) in
  step-arg / interpolation contexts. See
  `docs/sol-sflow-parity.md` for the full mapping and the
  remaining cross-language divergences (Sflow has no
  `for-in`; Sflow built-ins return `"true"` / `"false"`
  instead of typed bools; nested lists / maps are not yet
  supported in either language).
- **`try / catch` recovery shipped (both languages).** SOL
  added `try { … } catch <kind> { … }` with the same kind
  taxonomy as Sflow (`timeout`, `mesh_error`,
  `policy_denied`, `responder_error`, `any`), plus
  `error_kind()` / `error_cause()` / `error_retry_hint()`
  built-ins inside catch blocks and a `rethrow;` statement.
- **String interpolation `{{var}}` shipped (SOL).** Lowers to
  a parser-time concat chain. Sflow's `${var}` is unchanged.
- **`delegate` / `send` sugar shipped (SOL).** Soft-keyword
  forms that lower to `remote_call("coord",
  "delegate.spawn", …)` and `remote_call("coord",
  "msg.send", …)` respectively.
- No `match`-style branching (Sflow has if/elif/else; SOL has
  Rust-like if/else).
- No async (the executors are synchronous; no yield).
- No mid-flow pause / resume (see `docs/replay-model.md`).
- **(SOL only)** No types beyond `str` for `remote_call` args
  (function-typed `int`/`float`/`char`/`bool` exist but don't
  cross the dispatcher boundary).
- **(SOL only)** No function composition (one `start()` per file).
- **(Sflow only)** No user-defined functions — flows are a flat
  statement list.
- **(Sflow only)** No `for x in lst { … }` — list iteration uses
  `loop N times` + `list_get(lst, "${loop.iter}")`.
- No nested lists / maps in either language (a list of lists
  is parseable but the built-ins flatten on read).
- No regex captures (Sflow's `matches` is boolean only).
- No multi-line string literals (newline inside `"..."` rejected).

### 10.3 Policy today

`PolicyEngine` in `relix-core::policy`:

```toml
[admit]
groups = ["chat-users", "tool-users"]

[[rules]]
name = "chat_users_chat"
method = "ai.chat"
allow_groups = ["chat-users"]
```

Two-stage evaluation:
1. `[admit] groups` — node-level filter. If set, caller must hold one.
2. Per-method `[[rules]]` — first matching rule wins. Default deny.

Each `Decision` is `Allow { matched_rule }` or `Deny { reason,
matched_rule }`. The admission pipeline also has a `RequireApproval`
path via the agent gate (`GateDecision::RequireApproval`) and the
`always_require_methods` allowlist in `[approval]` — these are wired
as of 0.4.1 (see §8). The `PolicyEngine` itself remains allow/deny only;
the approval path is an additional step in the dispatch pipeline that
runs before policy.

### 10.4 What's missing for the Active-Directory-grade vision

The user's framing (mentioned in this session) is "a policy + identity
system as rich as Active Directory" — not what's in the codebase
today. Specifically missing:

- **Categorical permissions.** Now shipped via the agent gate (§8):
  per-agent `deny_categories` / `allow_categories`, risk ceiling,
  surface allowlist. Still not in the `PolicyEngine` TOML itself.
- **Resource-level permissions.** "Can write to `~/inbox/` but not
  `/secrets/`" is not expressible. Only method-level.
- **Time-bounded / standing approvals.** Standing approvals are shipped
  (`standing_approvals` table, `agent.standing_approval.*` caps).
  Time-bounded token TTL is also real (per-agent `approval_timeout_secs`).
- **Per-call approval prompts.** Now wired: `GateDecision::RequireApproval`
  is produced by the agent gate and handled by the bridge's
  `AgentGateBindings::on_require_approval` closure.
- **Group hierarchies.** Groups are still flat strings. No transitive
  membership is implemented.
- **Cedar-grade policy DSL.** Current policy is a thin allowlist. The
  source comments name Cedar as the Gate-2 target.
- **Delegation chains.** Only the org root can sign IdentityBundles.
  No "alice grants bob temporary access" workflow. SIMP-002.
- **Revocation.** Only expiry. No CRL. SIMP-003.
- **Audit-aware policy.** The policy engine sees `(principal, method)`
  but not "this principal already called X 100 times in the last
  hour". Rate-aware / quota-aware policy is not implemented.
- **Cross-node policy propagation.** Each node loads its own policy
  TOML at startup. Operator-visible single source of truth + hot
  reload is not implemented.

---

## 11. THE PLUGIN / ECOSYSTEM VISION

### 11.1 What exists today (0.4.1)

**A subprocess-based plugin system ships** (see §6.1d). `node_type =
"plugin_host"` scans a `plugin_dir`, spawns each plugin binary,
negotiates the `relix-plugin-v1` HTTP/JSON protocol over a TLS
loopback, and registers each declared capability on the dispatch bridge.
The plugin host enforces RLIMIT_AS / RLIMIT_CPU / RLIMIT_NOFILE sandbox
limits on POSIX; on Windows the limits are a no-op (enforced by
`ensure_enforceable()`). Optional publisher-key Ed25519 signature
verification on manifests lets operators require signed plugins.

Three other plugin-like primitives remain:

- **`CapabilityDescriptor`** as the unit of capability advertising.
- **SOL / Sflow / YAML flows** as orchestration scripts that consume
  capabilities without extending them.
- **Policy files** as deployment-time access control over any capability
  (built-in or plugin-provided).

The note from `docs/plugin-foundations.md` about "no plugin loading"
reflects the pre-0.4.1 state and is now stale for the subprocess layer.

### 11.2 What is still missing for a full outside-app-as-a-node ecosystem

The subprocess plugin system closes the sandbox and stable-ABI gaps for
the common case. What remains for a production plugin ecosystem:

- **WASM runtime.** The subprocess model requires a native binary. A
  WASM sandbox would allow language-agnostic, lighter-weight plugins
  without spawning OS processes. Deliberate non-goal for now.
- **Automatic restart on crash.** A panicking plugin stays down until
  the operator calls `plugin.reload`. No watchdog today.
- **Remote plugin discovery.** Plugins must be on the same host as the
  `plugin_host` node; no DHT or URL-based install. DHT remains inert
  (SIMP-007).
- **Marketplace hosted infrastructure.** Hosted registry, signing CA,
  payment processor, and web frontend are permanently
  CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL (GAP 19).
- **`CapabilityDescriptor.major_version` enforcement.** The runtime
  registers without checking version compatibility.

### 11.3 Architectural constraints any future plugin system must respect

From `docs/plugin-foundations.md`:

- **M1.** The admission pipeline cannot be bypassed. Every plugin call
  flows through identity → policy → handler → audit.
- **M2.** Plugins cannot grant themselves trust. The org-root key
  remains the only signer.
- **M3.** Plugins are auditable from source. Any distribution
  mechanism must keep the source available to operators.

---

## 12. HONEST GAPS

**0.4.1 update (2026-06-01):** Several items below have shipped since the
original audit. Each entry is annotated with its current status.

What's missing for Relix to be "real" — the gaps that block actual
deployment as a multi-agent operating layer rather than a developer
demo. Ranked by impact.

### 12.1 ~~No agent-employee permission model~~ **[SHIPPED — see §8]**

~~The single largest gap. Every "agent" today is just an
IdentityBundle in some groups. There is no:
- Per-agent permission scope expressed in categorical terms
- Approval flow for sensitive actions
- Agent status (active / suspended / disabled)
- Agent profile dashboard surface
- Standing approvals

Without this, Relix is "OpenAI-shim + tools" not "operating layer for
many agents". Design proposal exists (§8); implementation is zero.~~

As of 0.4.1 this is fully shipped end-to-end (see §8 for detail):
agent profile store, coordinator capabilities, bridge HTTP routes,
dispatch-pipeline integration, Telegram approval bridge, and dashboard
`#/agents` + `#/approvals` pages. Honest remaining limits: surface
field is operator-asserted (not cryptographically bound until SIMP-002),
multi-approver workflows are not yet in scope (single-approver ships),
and the dashboard does not push real-time approval notifications.

### 12.2 No mid-flow pause / resume

The SOL VM is synchronous. A capability call cannot say "I'm waiting
for human input — pause this flow". The `awaiting_input` task status
exists but only the *task* pauses; the SOL VM that initiated the call
is already gone. For any agent workflow that needs "the agent should
check with me before doing X", this is the blocker. Reusing
`awaiting_input` is the approach the agent-employee proposal sketches,
but it touches the VM contract.

### 12.3 Audit log is local-only

Each controller maintains its own hash-chained audit log. There is no
aggregator, no shipping to a SIEM, no cross-node single source of
truth. For a real deployment, **all** audit verification requires
walking every node's log and correlating by `request_id` /
`trace_id`. Operators are explicitly expected to ship logs out.

### 12.4 Identity has one delegation level

Compromised org root = compromised mesh. No Intermediate Authority.
No CRL. No revocation gossip. Bundle lifetime is the only mitigation
(default 24h). For real multi-team / multi-app deployments, this is
insufficient.

### 12.5 ~~No plugin / dynamic load~~ **[SHIPPED — see §6.1d]**

~~Every capability is compiled in. A third-party tool wanting to
register itself as a Relix capability has to fork the repo. No WASM
sandbox, no signed plugin manifest format, no marketplace. The
existing `CapabilityDescriptor` is the right primitive — but the
loading + sandbox layers are not built.~~

As of 0.4.1 the subprocess-based plugin system ships (§6.1d):
`node_type = "plugin_host"`, `plugin.toml` manifests with optional
Ed25519 publisher-key signing, sandbox limits (RLIMIT_AS / RLIMIT_CPU /
RLIMIT_NOFILE on POSIX), TLS loopback transport, SQLite registry, four
management capabilities, bridge + dashboard + CLI surfaces, Python and
Rust SDK examples. Remaining non-goals (deliberate): no WASM runtime,
no auto-restart on crash, no remote plugin discovery, no marketplace
hosted infrastructure (CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL per
GAP 19).

### 12.6 ~~No vector / semantic memory~~ **[SHIPPED — see §9 and Part 6]**

~~Memory is SQLite + FTS5 keyword search. No embeddings. No per-task
memory. No cross-session synthesis. For any agent that needs to
remember things in a way that survives session boundaries, this is
not enough.~~

As of 0.4.1 the four-layer memory system ships: Raw (SQLite+FTS5),
Semantic (Qdrant vector, per-tenant collections), Observation, and Model
layers with bi-temporal validity. Background curator, layer promoter,
consolidation archiver, anomaly scorer, quarantine flow, integrity
auditor, inspector edit/freeze/export, and dialectic synthesis are all
real. Per-tenant Qdrant isolation is enforced when
`[memory.qdrant] tenant_isolation = true`. Remaining honest gaps:
scope-to-context is deferred (needs a `scope` column + vocabulary
decision), hard-delete cascade uses `invalidate` not physical delete,
and a separate low-priority Qdrant archive segment is infeasible without
a breaking schema change.

### 12.7 ~~Streaming is fake~~ **[SHIPPED — see §6.5; SIMP-019 closed]**

~~The bridge consumes the AI provider's stream eagerly into a buffer,
then slices the buffer into 24-byte SSE chunks. For Open WebUI this
is invisible; for latency-sensitive UIs it's a real ceiling. SIMP-019.~~

As of 0.4.1 end-to-end token streaming is real and opt-in via a single
config line. Tokens flow from the provider's SSE response through the
libp2p `/relix/rpc/stream/1` substream, the SOL VM's
`remote_call_stream` opcode, and the bridge's SSE response to the HTTP
client with no intermediate materialisation. The same admission pipeline
(identity → agent gate → policy → access broker → audit) runs on the
streaming path. See §6.5 for the full architecture and test coverage.

### 12.8 ~~Telegram is scaffold-only~~ **[SHIPPED — see §6.1; Discord §6.1b; Slack §6.1c; Email §7.7]**

~~The dashboard has a Telegram settings page. The crate has a `BotApi`
trait. There is no live HTTPS implementation and no controller
binary wiring. Operators cannot actually send a Telegram message
through Relix today.~~

As of 0.4.1 all four channels are fully wired. Telegram ships a live
HTTPS client (reqwest + rustls), webhook mode, voice transcription via
`audio_peer`, and operator approval notifications. Discord and Slack
ship REST polling clients with persistent watermark stores. Email ships
SMTP/IMAP/DKIM with template support and OAuth2. All four channels
register `approval_send` for the OOB approval delivery matrix.

### 12.9 No DHT-based discovery

`peers.toml` is the only source of peer addresses. The bridge
discovers capabilities through `node.manifest` calls to known peers,
but cannot find new peers from the network. SIMP-007 / -017.

### 12.10 ~~No cost-aware throttling~~ **[PARTIALLY SHIPPED]**

~~`CapabilityDescriptor::cost_class` exists but the runtime does not
read it. A caller that floods `ai.chat` burns the provider's per-key
budget; the policy engine has nothing to say about it.~~

As of 0.4.1 a `BudgetEnforcer` enforces per-agent and deployment-wide
daily/hourly spend caps before dispatch. A `CostSpikeDetector` builds
durable baselines and fires `ProviderCostSpike` alerts. An
`AlertEngine` evaluates error-rate, P95 latency, cost/hour, and
zero-success thresholds with a `MultiChannelAlertSink`. Cost-class-aware
per-method rate throttling (the original gap) is still not implemented
— the policy engine remains allow/deny only. The BudgetEnforcer enforces
at the spending level; sub-method cost-class throttling is deferred.

### 12.11 No replay-debug UX

The replay primitive exists (W2-001) — operators can re-run a task and
the dashboard shows duration deltas. There is no side-by-side
chronicle comparison, no event-level diff, no per-step screenshot
diff. For "why did this run fail and the next succeed", operators
read the chronicle by hand. Design exists (paused per user request).

### 12.12 ~~Manifests are not signed~~ **[PARTIALLY SHIPPED — see §6.6]**

~~A peer can lie about its own capabilities. The bridge trusts what it
receives. For any deployment where mesh peers are not all under one
administrator, this is unsafe. SIMP-006.~~

As of 0.4.1 manifests are signed with Ed25519 + TOFU pinning
(`SignedManifest`). The signer pubkey is cross-checked against the
Noise-authenticated PeerId so self-asserted keys cannot bypass the
crypto boundary. The full `BundleType::NodeManifest` bundle chain
described in the RELIX-4 spec is a Gate 2 upgrade; the alpha signing
mechanism provides practical integrity for single-operator deployments.

---

## Appendix A — Crate map

| Crate | Purpose |
| --- | --- |
| `relix-core` | Shared substrate: codec, types, bundle, identity, policy, eventlog, audit, capability, redact, retry, router types. Zero unsafe. |
| `relix-runtime` | Mesh runtime: libp2p transport, SOL VM with `remote_call`, dispatch bridge, manifest exchange, node implementations (memory, ai, tool, coordinator, router). |
| `relix-controller` | Thin daemon binary. Just `relix_runtime::controller_runtime::run(&args.config).await`. |
| `relix-cli` | Developer + operator CLI. 15 subcommands across libp2p dial-and-call and HTTP-to-bridge. |
| `relix-flow-inspect` | Read flow event logs + audit logs. `--replay-verify` walks hash chains and verifies signatures. |
| `relix-web-bridge` | HTTP front: chat shim, OpenAI shim, dashboard host, task bridge, observability proxies. ~30 modules. |
| `relix-telegram` | Telegram channel node: live HTTPS client (reqwest+rustls), long-poll + webhook mode, slash commands, approval notifier, voice transcription path. |
| `relix-discord` | Discord channel node: REST polling client, persistent watermark store, approval_send. |
| `relix-slack` | Slack channel node: REST polling client, Block-Kit messages, historical-message filter, approval_send. |
| `relix-embedded` | In-process runtime for host-app embedding: `RelixEmbedded` builder, chat + memory_ingest + memory_search. No libp2p or bridge. |

## Appendix B — Key file pointers

| Subject | File |
| --- | --- |
| Identity bundle / VerifiedIdentity | `crates/relix-core/src/identity.rs` |
| Policy engine | `crates/relix-core/src/policy.rs` |
| Capability descriptor + RiskLevel | `crates/relix-core/src/capability.rs` |
| Audit format | `crates/relix-core/src/audit.rs` |
| Eventlog (per-flow signed log) | `crates/relix-core/src/eventlog.rs` |
| Dispatch bridge / admission pipeline | `crates/relix-runtime/src/dispatch/mod.rs` |
| SOL VM | `crates/relix-runtime/src/sol/` |
| Flow runner | `crates/relix-runtime/src/flow_runner.rs` |
| Controller runtime entry point | `crates/relix-runtime/src/controller_runtime.rs` |
| Node impls | `crates/relix-runtime/src/nodes/{memory,ai,tool,coordinator,router}.rs` (or `mod.rs`) |
| HTTP routes | `crates/relix-web-bridge/src/main.rs` |
| Dashboard HTML + JS | `crates/relix-web-bridge/src/dashboard.html` |
| CLI top level | `crates/relix-cli/src/main.rs` |
| Mesh boot script (Windows) | `scripts/relix-mesh-up.ps1` |
| Mesh boot script (POSIX) | `scripts/relix-mesh-up.sh` |
| End-to-end smoke (bash) | `scripts/demo-smoke.sh` |
| Decisions pending | `docs/internal/decisions-pending.md` |
| Agent-employee proposal | `docs/proposals/agent-employee-permissions.md` |
| Approval delivery (OOB matrix) | `crates/relix-runtime/src/approval/` |
| Credential vault (AES-256-GCM/Argon2id) | `crates/relix-runtime/src/credentials/` |
| Session identity tokens | `crates/relix-runtime/src/identity/session.rs` |
| Agent gate (admission) | `crates/relix-runtime/src/admission/agent_gate.rs` |
| Layered memory + Qdrant | `crates/relix-runtime/src/nodes/memory/` |
| Knowledge share | `crates/relix-runtime/src/knowledge/` |
| Training pipeline + PII | `crates/relix-runtime/src/training/` |
| Confidence / self-consistency | `crates/relix-runtime/src/confidence/` |
| Planning + workflow engine | `crates/relix-runtime/src/planning/` + `crates/relix-runtime/src/workflow/` |
| Metrics + BudgetEnforcer + AlertEngine | `crates/relix-runtime/src/metrics/` |
| OTel export + two-sink observability | `crates/relix-runtime/src/observability/` |
| Plugin host + sandbox | `crates/relix-runtime/src/plugin/` |
| Manifest signing (SignedManifest) | `crates/relix-runtime/src/manifest/` |
| Signed manifests + TOFU pinning | `crates/relix-runtime/src/manifest/cache.rs` |

## Appendix C — How to read the chronicle event vocabulary

Defined in `docs/event-vocabulary.md` + emitted across
`crates/relix-runtime/src/nodes/coordinator/`. Categorized loosely
into:

- **Lifecycle**: `task.created`, `flow.started`, `task.completed`, `task.failed`, `task.cancelled`, `task.interrupted`
- **Attempt**: `task.attempt_started`, `task.attempt_finished` (with `failure_class` on the err path)
- **Retry**: `task.retry_requested`, `task.retry_suppressed`, `task.retry_exhausted`, `task.replayed_from`
- **Pause / freeze**: `task.pause_requested` / `_observed`, `task.resume_requested` / `_observed`, `task.freeze_requested` / `_observed` / `_propagated`, `task.unfreeze_requested`
- **Operator action**: `task.investigation_marked`, `task.investigation_cleared`, `task.operator_note`
- **Health (H-events)**: `task.thrash_detected`, `task.attempt_orphan_closed`, `task.terminal_summary`
- **Lineage**: `task.spawned_child`, `task.delegated_to`, `task.awaiting`
- **Capability**: `capability.invoked`

The dashboard's W2-003c chronicle filter chips bucket these into
`capability / attempt / error / retry / pause / lifecycle` for
operator scanning.
