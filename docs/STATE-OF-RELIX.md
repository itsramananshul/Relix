# STATE OF RELIX

**Audit timestamp:** 2026-05-21 · 403 commits on `main` · HEAD `bb14a6b`
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
**does not** do: no plugin loading, no DHT discovery,
no rate limiting beyond allow/deny. The differentiating posture is
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
| `memory.search` | Real | FTS5 query across all turns |

### 3.3 `ai` node

| Method | Status | What it does |
| --- | --- | --- |
| `ai.chat` | Real | Provider-routed completion via `[ai] provider = ...` |

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
| `task.spawned_child` / `task.delegated_to` / `task.awaiting` | Real chronicle events |

Plus a long list of chronicle event types (`task.thrash_detected`,
`task.terminal_summary`, `task.attempt_orphan_closed`,
`task.retry_requested` / `_exhausted` / `_suppressed`,
`task.pause_requested` / `_observed`, `task.resume_*`, `task.freeze_*`,
`task.investigation_marked` / `_cleared`, `task.operator_note`,
`task.replayed_from`, `task.failed`, `task.completed`,
`task.cancelled`, `task.interrupted`, `task.attempt_started` /
`_finished`, `flow.started`, `capability.invoked`).

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
[cron.md](cron.md) for the full design.

### 3.11 `router` node — control plane

CBOR-encoded (not pipe-delimited like the rest).

| Method | Status | What it does |
| --- | --- | --- |
| `router.heartbeat` | Real | Controllers push liveness + caps every 60s |
| `router.network_summary` | Real | Operator-facing mesh overview |
| `router.session_list` | Real | Cross-peer session browser |
| `router.log` | Real | Controllers push structured log lines (bounded ring 10k) |

Reaper loops: stale-peer flip (90s), expired-session drop (300s).

### 3.12 What is **not** a capability

The Telegram channel scaffold (`relix-telegram` crate) ships
config + identity-derivation + a `BotApi` trait + a `MockBotApi` test
double — but **no live HTTPS implementation and no controller binary
wiring**. There's no `telegram.*` method registered today.

---

## 4. WHAT THE DASHBOARD SHOWS

Served by `relix-web-bridge` at `GET /dashboard` as a single static
HTML file with inline JS (no build step). Twelve top-level pages, each
addressable via `#/<route>` and `data-page="<route>"` in
`crates/relix-web-bridge/src/dashboard.html`.

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

- Bot token status, webhook config, allowed user groups, recent-message ring. **Scaffold UI** — the live HTTPS client is not implemented.

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
cards. Setup is documented in `docs/telegram.md`. Boot via
`scripts/relix-mesh-up.ps1` with `$env:RELIX_TELEGRAM = "1"` and
`$env:RELIX_TELEGRAM_BOT_TOKEN = "<token>"`.

### 6.2 Playwright browser backend — scaffold

`[tool.browser] backend = "playwright"` selects a scaffold that returns
`BackendNotConnected` on every operation (`navigate` / `click` /
`type_text` / `wait_for_selector` / `screenshot`). The headless_chrome
and webdriver backends are real; playwright is not. The sidecar
protocol is not designed.

### 6.3 MCP HTTP transport

`tool.mcp.invoke` over HTTP transport returns `RuntimeNotConnected`.
Only stdio is implemented (lazy spawn, JSON-RPC over child process
pipes). Operators can register HTTP MCP servers in config but cannot
invoke them.

### 6.4 OpenAI shim drops fields

`POST /v1/chat/completions` accepts the full request shape but silently
ignores: `temperature`, `top_p`, `n`, `presence_penalty`,
`frequency_penalty`, `logit_bias`, `tools`, `tool_choice`,
`response_format`, `seed`, `stop`, `stream_options`. The bridge sends
only `model` + `messages` to the AI node. SIMP-020.

### 6.5 Streaming is chunk-sliced, not provider-native

`POST /chat/stream` and `stream:true` slice the **already-materialised**
reply into 24-byte SSE chunks. The AI provider's stream (if any) is
consumed eagerly first. SIMP-019.

### 6.6 Manifests are not signed

`NodeManifest` is sent as plain CBOR. A peer can lie about its own
capabilities; the bridge trusts what it receives. Gate 2 wraps in a
`Bundle(BundleType::NodeManifest)`. SIMP-006.

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

## 7. WHAT IS PLANNED BUT NOT STARTED

Anything that has a docs / proposal / decision-pending entry but no
implementation.

### 7.1 Proposals

**Only one proposal file ships today** — written and committed in
this session:

| Proposal | Status |
| --- | --- |
| `docs/proposals/agent-employee-permissions.md` | Design pass, awaiting sign-off (see §8) |

### 7.2 Replay UX V2

Design pass drafted in this session's chat (not written to a file).
Three slices proposed:
- **Slice A** — lineage breadcrumb + side-by-side chronicle comparison
- **Slice B** — event-level diff chips + screenshot side-by-side + outcome-coloured retry pills
- **Slice C** — replay endpoint extensions (`overrides`, `mode: execute|dryrun`)

User explicitly paused this track to prioritize the agent-employee
permission model.

### 7.3 Decisions-pending entries (`docs/internal/decisions-pending.md`)

Twelve numbered D-### entries with operator-recommendations. Recent
status: D-001 through D-007 marked `defer`. D-008 through D-010 marked
`shipped`. Open / future: token throttling, MCP transport completion,
multi-org boundaries — not actively in a slice.

### 7.4 Other "docs without code" items

- `docs/channel-node-architecture.md` — Telegram channel architecture; only the scaffold is built (see §6.1).
- `docs/replay-model.md` — names the SOL VM as synchronous, frames why pause-and-resume is hard.
- `docs/plugin-foundations.md` — explicitly **not** an implementation plan; sketches constraints any future plugin system must respect.
- `docs/multi-node-bringup.md` — describes the `relix-mesh-up.ps1` flow, not new architecture.
- `docs/dashboard-redesign.md` — design refs for sections of the current dashboard; some redesign items are implemented, others not (notably the deeper Telegram settings page).
- `docs/production-checklist.md` — operational gates for a hypothetical production deploy. Not a track.

### 7.5 Specs / SIMP entries deferred to Gate 2

Mentioned throughout `docs/current-limitations.md` and in source comments:
- SIMP-002 — Intermediate Authority layer for identity bundles
- SIMP-003 — CRL / revocation gossip
- SIMP-006 — manifest signing
- SIMP-007 / -017 — DHT / capability gossip
- SIMP-016 — typed CDDL replaces `String`-shaped args at SOL boundaries
- SIMP-018 — typed flow arguments (replaces character-level template substitution)
- SIMP-019 — provider-native streaming
- SIMP-020 — OpenAI shim field coverage

---

## 8. THE AGENT EMPLOYEE PERMISSION MODEL

**Status: proposal-only.** Zero implementation.

The full proposal lives at `docs/proposals/agent-employee-permissions.md`
(715 lines, written and committed in this session).

What exists in code today that the proposal builds on:

- **`IdentityBundle`** with `subject_id`, `name`, `org_id`, `groups`,
  `role`, `clearance`, and a reserved-but-unused `supervisors: Vec<String>` field.
- **`VerifiedIdentity`** propagating identity into every dispatch.
- **`PolicyEngine`** with two-stage admission (`[admit] groups` +
  per-method `[[rules]]`). A `RequireApproval` variant is reserved in
  source comments but not implemented.
- **`CapabilityDescriptor`** with `categories: Vec<String>`,
  `sensitivity_tags: Vec<String>`, `risk_level: RiskLevel`,
  `environment_requirements: Vec<String>`. These are the building
  blocks the proposal would consume for categorical permissions.
- **`task.replayed_from` / `awaiting_input` task status** — the
  proposal's pause-and-resume approval flow reuses these.
- **W2-007 policy-hardening surface** — `node.policy.simulate` (W2-007a),
  `node.policy.recent_denials` (W2-007d) — the proposal would
  use these for the dashboard "agent profile" view.

What is **not** in code:

- No agent record type (the AIC bundle is the closest analogy)
- No status field (active / suspended / disabled)
- No categorical permission gating
- No approval flow — `Decision::RequireApproval` does not exist
- No `surface` field on request envelopes
- No standing-approvals concept
- No `/v1/agents` or `/v1/approvals` bridge routes
- No `#/agents` or `#/approvals` dashboard pages

The proposal's five-phase build order would be a multi-session
implementation. Phase 1 (agent record + status / surface / risk-ceiling
gates) is the smallest unit and is operator-observable on its own.

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

### 9.3 What is missing

- **No vector embeddings.** Search is FTS5 keyword only over
  per-turn data. The new persistent-memory layer has no
  search at all (operators / agents look it up by
  subject_id).
- **No cross-agent memory sharing.** Each `subject_id` row is
  isolated; no team / department / org grouping.
- **No per-session scope** on persistent memory. Memory is
  global per-agent across all sessions.
- **No auto-eviction.** Operators must remove old entries
  manually (or rely on agent self-curation when that lands).
- **No background curator.** Hermes runs a scheduled review
  loop that archives stale skills and consolidates memory
  entries via an auxiliary LLM. Relix does not. Future track.
- **No write-time PII validation.** Bodies are accepted
  verbatim; secrets / PII / huge documents are persisted
  as-is.
- **No memory time-bounding.** A 6-month-old session and a
  fresh session look identical to `recent_for_session`.

---

## 10. SOL AND POLICY

### 10.1 SOL today

SOL is a small imperative DSL with:
- Variables (`let x: str = ...`)
- String concatenation (`+`)
- `print(x)` (writes to stdout for `flow-run`)
- `return x`
- `function start() -> str { ... }` — the single entry point
- **`remote_call(peer_alias_or_capability_uri, method, args)`** —
  the mesh primitive

That's it. No conditionals. No loops. No types beyond `str`. No
collections. No escapes in string literals.

Argument convention: pipe-delimited strings (`session|prompt|history`).
SIMP-016 — the alpha keeps the SOL ↔ handler boundary as `String` to
avoid inventing a SOL type system. Gate 2 replaces this with typed
CDDL.

Bridge templates substitute `{{SESSION}}`, `{{MESSAGE}}`, `{{TOOL_URL}}`
into `.sol` files before compiling. The substitution validator rejects
`"`, `|`, and `\n` in user input. SIMP-018.

### 10.2 What SOL cannot do

- No branching (`if`, `match`)
- No loops (`while`, `for`)
- No error handling (a failed `remote_call` halts the VM with
  `VM_ERROR_SENTINEL`; subsequent statements do not run)
- No data structures (`list`, `map`)
- No async (the VM is synchronous; no yield)
- No mid-flow pause / resume (see `docs/replay-model.md`)
- No types beyond `str`
- No function composition (one `start()` per file)

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
matched_rule }`. A `RequireApproval` variant is reserved but not
implemented.

### 10.4 What's missing for the Active-Directory-grade vision

The user's framing (mentioned in this session) is "a policy + identity
system as rich as Active Directory" — not what's in the codebase
today. Specifically missing:

- **Categorical permissions.** Today policies are per-method. There is
  no "this agent can browse but not pay" abstraction; that's the
  agent-employee proposal (§8).
- **Resource-level permissions.** "Can write to `~/inbox/` but not
  `/secrets/`" is not expressible. Only method-level.
- **Time-bounded / standing approvals.** Not implemented.
- **Per-call approval prompts.** No `RequireApproval` decision.
- **Group hierarchies.** Groups are flat strings. No transitive
  membership ("operators inherits from chat-users").
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

### 11.1 What exists today

`docs/plugin-foundations.md` is explicit: **today there is no plugin
loading**. The capability set on a controller is determined at compile
time. Three things in the codebase are plugin-like:

- **`CapabilityDescriptor`** as a unit of discovery. A plugin would
  ship a `(method_name, descriptor, handler)` triple. The dispatch
  bridge already accepts these via `register()`.
- **SOL flows** as composable units. Drop a new `.sol` file into
  `flows/` and reference it from any controller config without
  rebuilding. **Flows are NOT plugins** — they're orchestration
  scripts that consume plugins. An attacker who can write a `.sol`
  file is bounded by the admission pipeline; an attacker who can
  write a capability handler is not.
- **Policy files** as deployment-time configuration. Adding /
  removing a capability from `requires_groups` is a non-code
  deployment change.

### 11.2 What is missing for an outside-app-as-a-node ecosystem

For a third party to plug in their app as a Relix node, they would
need:

- **A stable wire ABI for capability registration.** Today handlers
  are Rust `async fn`s registered via `DispatchBridge::register`. No
  dynamic loading, no C ABI, no WASM sandbox, no gRPC handler-server
  pattern.
- **A trust + signature model.** Outside-app handlers would need to
  prove they came from a trusted source. The proposal's posture is
  "a plugin manifest signed by a trusted issuer", but no such
  manifest format exists yet.
- **A sandbox.** The admission pipeline is policy-level, not
  process-level. A misbehaving handler can reach the host. A real
  ecosystem needs at minimum a WASM sandbox or sub-process
  isolation.
- **A package format.** No `.relix-plugin` archive format, no
  signing convention, no install path.
- **A registry / discovery mechanism.** No marketplace, no DHT-based
  capability advertisement (DHT is configured in libp2p but inert —
  SIMP-007).
- **Versioning + compatibility checking.** `CapabilityDescriptor` has
  `major_version` but the runtime does not enforce it across plugin
  loads.

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

What's missing for Relix to be "real" — the gaps that block actual
deployment as a multi-agent operating layer rather than a developer
demo. Ranked by impact.

### 12.1 No agent-employee permission model

The single largest gap. Every "agent" today is just an
IdentityBundle in some groups. There is no:
- Per-agent permission scope expressed in categorical terms
- Approval flow for sensitive actions
- Agent status (active / suspended / disabled)
- Agent profile dashboard surface
- Standing approvals

Without this, Relix is "OpenAI-shim + tools" not "operating layer for
many agents". Design proposal exists (§8); implementation is zero.

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

### 12.5 No plugin / dynamic load

Every capability is compiled in. A third-party tool wanting to
register itself as a Relix capability has to fork the repo. No WASM
sandbox, no signed plugin manifest format, no marketplace. The
existing `CapabilityDescriptor` is the right primitive — but the
loading + sandbox layers are not built.

### 12.6 No vector / semantic memory

Memory is SQLite + FTS5 keyword search. No embeddings. No per-task
memory. No cross-session synthesis. For any agent that needs to
remember things in a way that survives session boundaries, this is
not enough.

### 12.7 Streaming is fake

The bridge consumes the AI provider's stream eagerly into a buffer,
then slices the buffer into 24-byte SSE chunks. For Open WebUI this
is invisible; for latency-sensitive UIs it's a real ceiling. SIMP-019.

### 12.8 Telegram is scaffold-only

The dashboard has a Telegram settings page. The crate has a `BotApi`
trait. There is no live HTTPS implementation and no controller
binary wiring. Operators cannot actually send a Telegram message
through Relix today.

### 12.9 No DHT-based discovery

`peers.toml` is the only source of peer addresses. The bridge
discovers capabilities through `node.manifest` calls to known peers,
but cannot find new peers from the network. SIMP-007 / -017.

### 12.10 No cost-aware throttling

`CapabilityDescriptor::cost_class` exists but the runtime does not
read it. A caller that floods `ai.chat` burns the provider's per-key
budget; the policy engine has nothing to say about it.

### 12.11 No replay-debug UX

The replay primitive exists (W2-001) — operators can re-run a task and
the dashboard shows duration deltas. There is no side-by-side
chronicle comparison, no event-level diff, no per-step screenshot
diff. For "why did this run fail and the next succeed", operators
read the chronicle by hand. Design exists (paused per user request).

### 12.12 Manifests are not signed

A peer can lie about its own capabilities. The bridge trusts what it
receives. For any deployment where mesh peers are not all under one
administrator, this is unsafe. SIMP-006.

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
| `relix-telegram` | Telegram channel scaffold (config, identity-derivation, BotApi trait, MockBotApi, session-store). No live HTTPS implementation. |

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
