# Continuation State — Wave 1 Real Tool Backends

This file is the authoritative handoff for the next Claude session.
Treat the **WHEN USER SAYS CONTINUE, START HERE** block at the end
as the resume command.

---

## Repository state at the checkpoint

- **Branch:** `main`
- **HEAD:** `9b26baf feat(cli): W2-008i — relix-cli ops snapshot for incident attachments`
- **Status:** clean working tree, branch up to date with `origin/main`
- **Remote:** `origin` → `https://github.com/itsramananshul/Relix.git`
- **Wave 1 status: CLOSED.** All decisions resolved.
- **Wave 2 status: IN PROGRESS.**
  - **W2-001 (Replay UX):** substantively CLOSED — per-step
    duration in timeline (W2-001a) + `task.replay` capability
    (W2-001b) + `POST /v1/tasks/:id/replay` bridge route
    (W2-001c) + Replay button (W2-001d) + replay banner on
    derived task detail (W2-001e). Replay-diff mode pending.
  - **W2-002 (Browser):** CLOSED end-to-end — trait
    extension + HC live + WD live + screenshot-on-failure +
    event-trace logging (W2-002a–e); `tool.browser.capture_read`
    runtime capability (W2-002f) → bridge proxy
    `/v1/browser/captures/:filename` (W2-002g) → dashboard
    inline thumbnail rendering (W2-002h). PW sidecar
    click/type/wait remains the only deferred piece.
  - **W2-005 (Failure system):** substantively shipped via
    Wave 1.
  - **W2-006 (Observability):** CLOSED end-to-end (runtime →
    bridge → dashboard → CLI), plus latency-shape sparkline
    extension (W2-006d runtime ring + bridge parser + dashboard
    SVG; W2-006e CLI Unicode-block sparkline).
  - **W2-003 (Dashboard UX):** substantively CLOSED —
    Metrics + What-If form (W2-003a/b) + chronicle category
    + text filters (W2-003c/d) + URL-hash persistence
    (W2-003e) + task-list time-window chips (W2-003f).
    Status pills still pending.
  - **W2-007 (Policy hardening):** CLOSED end-to-end (both
    pillars + CLI) —
    `node.policy.simulate` (W2-007a) → bridge proxy
    (W2-007b) → dashboard "What If" form (W2-007c);
    policy denial ring `node.policy.recent_denials`
    (W2-007d) → bridge proxy `GET /v1/policy/denials`
    (W2-007e) → dashboard "Recent denials" card (W2-007f);
    CLI mirrors `relix-cli ops {policy-simulate,
    policy-denials}` (W2-007g). Dry-run replay mode pending.
  - **W2-004 (SOLFlow):** started — `relix-cli sol templates`
    + `sol new --template ping --out path.sol` ship the
    quick-add workflow. Visual-editor sub-items (slash
    commands, drag-to-insert, variable picker, condition
    builder) deferred per spec ("DO NOT overcomplicate").
  - **W2-008 (Local-first):** CLOSED end-to-end —
    `relix-cli doctor` env health (W2-008a) +
    `scripts/demo-smoke.sh` bash smoke (W2-008b) +
    `relix-cli ops smoke` Rust smoke (W2-008c) +
    `relix-cli ops tail` live firehose tail (W2-008d) +
    Metrics page auto-refresh (W2-008e) +
    `relix-cli ops events --csv` spreadsheet dump (W2-008f) +
    multi-stage Dockerfile + ops walkthrough (W2-008g) +
    `relix-cli ops openwebui-setup` connection block
    (W2-008h) +
    `relix-cli ops snapshot` incident-response dump
    (W2-008i). One-command startup
    (`scripts/relix-mesh-up.sh`) predates this wave.
- **Workspace tests:** **1299 passing on default features**;
  feature-gated additions preserved (+5 HC live, +8 WD live).
  - relix-cli: 145 (+5 W2-008h port_from_bridge + models)
  - relix-policy: 72
  - relix-runtime: 742
  - relix-runtime router_node integration: 6
  - relix-telegram: 23
  - relix-cli bins: 2
  - relix-web-bridge: 307
  - bridge invariants: 3
- **Gates:** `cargo fmt --all` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean.

---

## What shipped this session (post-shutdown recovery)

15 milestones since the post-shutdown recovery, all green:

| Commit | Milestone | Concept |
|---|---|---|
| ca08428 | docs(internal) | recovered-execution-state + D-008 |
| 8ed8e05 | PH-FS-PARITY3 | tool.search_files `glob` mode (`*`/`**`/`?`) |
| 47b5db9 | PH-FS-PARITY4 | tool.fs.audit_recent + per-jail mutation ring |
| 1feb8ce | PH-TERM-SESSIONS | tool.terminal.sessions live run registry |
| acac673 | PH-MCP-PROTO | MCP JSON-RPC wire layer (no I/O) + D-009 logged |
| 512e727 | PH-TERM-AUDIT | tool.terminal.audit_recent completion ring |
| 9d4381d | PH-TERM-CANCEL | tool.terminal.cancel + manual drain refactor |
| c8d7fcb | PH-TERM-STREAM1 | tool.terminal.tail polling cursor |
| d5587d2 | PH-TERM-SPAWN | tool.terminal.spawn + validate_and_spawn refactor |
| 57c575d | PH-TERM-SHELL | tool.terminal.shell.{open,input,close} |
| cf2ea48 | PH-TERM-CONTROL | tool.terminal.shell.control + D-010 logged |
| e825be6 | PH-FS-FUZZY + PH-FS-TREE + PH-FS-STAT | filesystem Hermes parity |
| 4199b9b | PH-WEB-MARKDOWN | tool.web_extract `markdown` mode — HTML → Markdown structural conversion |
| 8632314 | PH-PDF-CHUNK | tool.text.chunk — general text chunker (paragraph > sentence > word > char) |
| 31c160e | PH-MCP-CLI | relix-cli mcp {servers,tools} — libp2p dial-and-call for MCP registry inspection |
| f642e52 | PH-TERM-CLI | relix-cli terminal {sessions,audit,cancel} — terminal observability + cooperative cancel |
| a307138 | PH-FS-AUDIT-FILTER | tool.fs.audit_recent JSON arg form with optional op filter |
| a27e4a3 | PH-CAP-RISK stage 1 | RiskLevel enum + field on CapabilityDescriptor + validator + CLI render |
| f570a9b | PH-CAP-RISK stage 2 | sweep every shipped descriptor with explicit risk tier |
| 4de4a53 | PH-CAP-RISK3 | relix-cli capability ls --risk filter (exact + at-or-above) |
| 8ae5ff1 | PH-BRIDGE-MCP | GET /v1/mcp/{servers,tools} bridge HTTP proxy for MCP registry |
| 10bb4d9 | PH-DASH-MCP | dashboard #/mcp page with server table + expandable tools per row |
| 155ca06 | PH-BRIDGE-MCP-INVOKE | POST /v1/mcp/invoke bridge proxy (502 until D-009 unblocks runtime) |
| aa37f66 | PH-WEB-POST | tool.web.post — POST + body + raw cookie header + Set-Cookie response capture |
| 8304d21 | PH-WEB-POST-RISK-CROSS | regression guards: web POST descriptors are Medium-tier + --risk medium+ surfaces tool.web.post |
| eb9c0d5 | PH-RISK-PIN-ALL | pin risk tier of every shipped tool descriptor (9 modules, 9 tests, all non-Unknown + tier-exact) |
| 685aa1e | PH-BRIDGE-MCP-AUDIT | bridge-side bounded ring for `POST /v1/mcp/invoke`; `GET /v1/mcp/audit`; dashboard card; classified `error_kind`; args content never recorded |
| 5fd8af4 | PH-CLI-MCP-AUDIT | `relix-cli mcp audit` — HTTP mirror of the bridge ring; `--bridge`, `--max`, `--raw`; padded table render; 3 forward-compat tests |
| 8cbaa2a | PH-BRIDGE-FS-AUDIT | `GET /v1/fs/audit` proxy for `tool.fs.audit_recent`; new `#/fsaudit` dashboard page with peer/op/max controls + table; kbd 6 reassigned; 400 on INVALID_ARGS; 8 parser tests + landmark |
| 3e8dc5e | PH-BRIDGE-TERM-AUDIT | `GET /v1/terminal/audit` proxy for `tool.terminal.audit_recent`; new `#/termaudit` dashboard page with status badge derived from exit/timed_out/cancelled; kbd 7 → termaudit (Configure → 8/9/0); 8 parser tests + landmark |
| 6c091fb | PH-CLI-AUDIT-MIRRORS | `relix-cli fs audit` (new fs module) + `relix-cli terminal audit-http` (HTTP sibling of libp2p audit); padded tables; status-badge derived from exit/flags; urlencode_token for op-filter safety; 10 wire-shape tests |
| 5cb3ab4 | PH-WEB-BLOCKLIST | `[tool] blocked_hosts` operator-curated hostname blocklist; new `HostBlocklist` type + `SsrfError::HostBlocked`; runs before scheme/DNS + on every redirect; exact-match-only (no subdomain widening) honesty contract; URLhaus refresh recipe in module doc; 9 tests + 14 existing call sites threaded |
| 5120dce | PH-DASH-BLOCKLIST | new `tool.web.blocklist_summary` Safe capability + `GET /v1/tool/blocklist` bridge proxy + dashboard card on `#/fsaudit` page (first-200 cap, sorted, honest "not live feed" note); 4 runtime + 8 bridge tests |
| 6d09544 | PH-BROWSER-FEATURES | refactored `browser.rs` → `browser/` directory with frozen `BrowserBackend` trait + three feature-gated backend modules (`browser-headless-chrome` / `-playwright` / `-webdriver` + `browser-all`); new `BrowserError::FeatureNotCompiled` variant; `ToolBackend::new` validate-on-construct prevents silent NoneBackend fallback; scaffold `with_label` surfaces operator-chosen backend name; D-008 flipped to "multi-backend plan accepted" pending PH-BROWSER-HC/PW/WD; 7 default + 3 feature-gated tests |
| b0dcaee | PH-CLI-WEB-BLOCKLIST | `relix-cli web blocklist` — HTTP mirror of `GET /v1/tool/blocklist`; sorted host list + `--max` cap + `--raw`; new `web` sibling under main.rs; 3 wire-shape tests; docs/capabilities.md updated |
| 39ca0b8 | PH-BROWSER-PW | live Playwright sidecar backend behind `browser-playwright` — Node + playwright-core over stdio JSON-RPC; embedded sidecar.js via include_str!; sync trait → async bridge via block_in_place + Handle::current().block_on; 5 tests incl. runtime-gated live navigate; subagent-authored, orchestrator-verified |
| b7f30c9 | PH-BROWSER-HC | live `headless_chrome` crate backend behind `browser-headless-chrome` — Chrome DevTools Protocol; lazy Browser launch; per-session Tab cached in Mutex<HashMap>; 5 tests incl. runtime-gated chromium-in-PATH probe; subagent-authored, orchestrator-verified |
| 1d24c46 | PH-BROWSER-WD | live `fantoccini` WebDriver backend behind `browser-webdriver` — sync→async bridge via block_in_place; new `webdriver_url` BrowserConfig field (default `http://127.0.0.1:9515`); lazy driver connect; 8 tests incl. runtime-gated `/status` probe; subagent-authored, orchestrator-verified |
| 6dcc670 | (fix) | HC + PW struct-literal `cfg()` test helpers updated with `..BrowserConfig::default()` so `--features browser-all` clippy + tests compile after the WD field addition |
| 2e706d1 | PH-BROWSER-D008-RESOLVE | flipped D-008 from "open" to "SHIPPED" in decisions-pending; Wave 1 status track A reflects all three live drivers behind feature flags |
| f7f4f2e | PH-DASH-BROWSER | `GET /v1/browser/sessions` bridge proxy + new `#/browser` dashboard page (peer-alias input, refresh, session table with truncated id + on-hover full id + status badge + "(no navigation yet)" empty current_url); 8 parser tests + 1 landmark |
| 0ab93a6 | PH-CLI-BROWSER | `relix-cli browser sessions` HTTP mirror of `GET /v1/browser/sessions`; padded table; new `browser` sibling under main.rs; 3 wire-shape tests |
| 2dc516c | PH-MCP-RUNTIME | live stdio MCP runtime closes D-009. New `mcp_stdio.rs` `McpStdioClient` (lazy subprocess spawn, mutex-serialised JSON-RPC, notification-tolerant read loop, kill_on_drop); `McpServerConfig.{command,args}`; `tool.mcp.invoke` + `tool.mcp.list_tools` now route through live client when transport=stdio; runtime tests +16; integration test against `@modelcontextprotocol/server-everything` via npx |
| 6f31093 | PH-TERM-PTY | portable-pty backend behind `--features terminal-pty` closes D-010. New `TerminalConfig.pty: bool`; `terminal.rs` → `terminal/` directory split; new `terminal/pty.rs` (~580 LOC); loud-fail at `ToolBackend::new` when `pty=true` and feature off; PTY path uniformly registered with the existing sessions/audit/cancel infrastructure; runtime tests +3 default / +2 more with feature on |
| b01ce75 | (docs) | Wave 1 CLOSED tally — D-008/9/10 status flips, full track-by-track wave-1 status table refreshed |
| 7b40a13 | PH-DEFER-DECISIONS | answered D-001/D-002/D-003/D-007 = defer; rationale captured in decisions-pending |
| 4b81913 | PH-ORIGIN-SURFACE | D-004 shipped — `origin_surface TEXT` column on tasks + `Coordinator::create` 8th param + `handle_create` parses 8th slot + TaskView field + 5 tests |
| 429f5b2 | W2-002a | BrowserBackend trait extension: `click` / `type_text` / `wait_for_selector` with default-impl BackendNotConnected; descriptors + handlers + manifest entries + 7 tests |
| a349c59 | W2-002b | HeadlessChromeBackend overrides defaults with live CDP click / type_into / wait_for_element_with_custom_timeout; per-call error reasons |
| 5a0e5ec | (D-004 follow-up) | `task.get` body emits `origin_surface=...` line so bridge `TaskDetail.header` map carries it to the dashboard |
| 27b33a0 | W2-002c | screenshot-on-failure: opt-in `[tool.browser] screenshot_on_failure_dir` persists a PNG of the failure state and appends `; screenshot=<path>` to the error reason — replay-friendly post-mortem aid |
| 4ea1311 | W2-002d | structured `tracing::info!` event traces on navigate / click / type_text / wait_for_selector handlers (`method`, `backend`, `session_id`, `elapsed_ms`, `outcome`, `reason`, ...) — text payload deliberately NOT logged (form-credential safety) |
| 7b213ba | docs | Wave 2 tally through W2-002d |
| 627329d | W2-002e | WebDriverBackend live click / type_text / wait_for_selector via fantoccini; wait_for_selector bypasses backend call_timeout so operator-supplied timeout_ms governs |
| f59d720 | W2-006a | CapStats extended with last/max/total/samples elapsed_ms; only handler.invoke timed (excludes admission overhead) |
| 6fc43b3 | W2-006b | `node.dispatch.stats` capability surfaces the snapshot over the dispatch wire; tab-delim row format; DispatchBridge.capability_stats now Arc<RwLock> so handlers can capture a cheap clone |
| 1c25965 | W2-006c | `GET /v1/dispatch/stats` bridge proxy + 6 parser tests; serde skip-if-none on last_error_at |
| 48a073a | W2-006d | `#/metrics` dashboard page renders the snapshot sorted by mean elapsed desc; visual cues for tail-ratio (max÷mean > 5x) + err count tier |
| ee71e3d | W2-001a | per-step duration ("+1.2s since prev") in chronicle timeline rows — operators spot which step took 8s |
| a643f99 | W2-006e | `relix-cli ops dispatch-stats` mirrors GET /v1/dispatch/stats — terminal twin of the Metrics dashboard panel |
| d9015a8 | docs | Wave 2 tally |
| 0fd853d | W2-007a | `node.policy.simulate` built-in capability — operators ask "what would the policy decide?" without invoking; synthetic VerifiedIdentity inherits caller identity but swaps groups; name suffix `:simulate` |
| 48fa803 | W2-007b | `GET /v1/policy/simulate` bridge HTTP proxy; `matched_rule` / `reason` Option-shaped with skip_serializing_if; 6 parser tests; 400 on missing method |
| 4e19f60 | W2-007c | Dashboard "Policy What If" card on Capabilities page; allow/deny badge + rule + reason render; Enter-to-submit + landmark test |
| 134c2b1 | W2-001b | `task.replay` capability + `TaskStore::replay_from` clones a task (title `(replay)`, fresh retry_count) + writes `retried_from` edge + `task.replayed_from` chronicle event; 5 tests |
| 6408ea4 | W2-001c | `POST /v1/tasks/:id/replay` bridge endpoint returns `{original_task_id, new_task_id}`; intervention_audit records the action |
| e12ad4b | W2-001d | Dashboard "Replay" button on task detail action bar with confirm-guard + auto-navigate to new task on success — W2-001 substantively closed end-to-end |
| cb430e9 | W2-004a | `relix-cli sol templates` + `sol new --template <name> --out <path>` — 6 baked-in workflow templates via include_str!; `--force` overwrite + parent-dir creation; 8 tests |
| 39114af | W2-008a | `relix-cli doctor` — bridge health probe + PASS/WARN/FAIL report; non-zero exit on FAIL for CI; 8 tests on the pure `evaluate()` function (zero peers / expired / flapping / missing coordinator) |

Plus 6 docs-only commits tallying each milestone into
`docs/internal/recovered-execution-state.md`.

Aggregate code change: ~4000+ LOC across `tool/fs.rs`,
`tool/terminal.rs`, `tool/mcp.rs`, `controller_runtime.rs`,
`docs/capabilities.md`, `docs/internal/hermes-capability-map.md`,
`docs/internal/decisions-pending.md`,
`docs/internal/recovered-execution-state.md`.

Runtime tests: 507 → 615 (+108). Workspace: 887 → 995 (+108).

---

## Wave 1 status by track

### A. Browser backend — SHIPPED (D-008 closed)

PH-BROWSER-FEATURES froze a clean `BrowserBackend` trait +
`browser/` directory layout; PH-BROWSER-HC / -PW / -WD then
landed three live drivers behind Cargo features
(`browser-headless-chrome` / `-playwright` / `-webdriver`,
plus `browser-all`). PH-BROWSER-D008-RESOLVE flipped D-008
from "open" to "shipped." Operator picks one at runtime via
`[tool.browser] backend = "..."`; selecting a backend whose
feature isn't compiled fails loudly at startup (no silent
NoneBackend fallback). Each driver is lazy on browser /
sidecar / driver launch — the tool node starts cleanly
when the runtime isn't present and surfaces the missing
runtime via a `BackendNotConnected { reason: "<backend>:
<cause>" }` envelope on the first call.

### B. MCP runtime — SHIPPED (D-009 closed)

PH-MCP-PROTO shipped the JSON-RPC wire layer first
(`mcp::proto` module, 12 tests). PH-MCP-RUNTIME then landed
a live `McpStdioClient` in `tool/mcp_stdio.rs` (~500 LOC)
that spawns the operator-configured server subprocess
lazily on first `tool.mcp.invoke`, serialises one request
at a time over a mutex, drains server-side notifications
between responses, and surfaces every failure as a typed
`mcp: <reason>` envelope. `McpServerConfig` gained `command`
+ `args`; `tool.mcp.list_tools` does a live `tools/list`
for stdio transports and falls back to the operator-declared
list on transport failure. HTTP transport still returns
`RuntimeNotConnected` (no fake fallback) — operators wanting
HTTP MCP file a follow-up. Integration test against
`@modelcontextprotocol/server-everything` via `npx` runs
end-to-end (~2.5s). D-009 flipped from "open" to "shipped"
in decisions-pending.

### C. Filesystem — substantial parity now

Shipped: read / write / append / list_dir / patch / patch_preview /
binary_sniff / audit_recent / search_files (name/content/glob) /
fuzzy_replace / tree / stat.

Gaps vs Hermes:
- `file_state` (per-session undo store) — needs a per-session
  storage layer that doesn't exist.
- batch operations wrapper — small.
- safe `delete` / `mkdir` / `move` / `rename` / `copy` — need
  policy decisions about which to surface; tool node node has no
  delete capability today.

### D. Terminal — SHIPPED including PTY (D-010 closed)

Default-features shipped:
- `tool.terminal.run` (sync, CW1)
- `tool.terminal.spawn` (background, PH-TERM-SPAWN)
- `tool.terminal.sessions` (live registry, PH-TERM-SESSIONS)
- `tool.terminal.cancel` (cooperative kill, PH-TERM-CANCEL)
- `tool.terminal.tail` (polling stream, PH-TERM-STREAM1)
- `tool.terminal.audit_recent` (completion ring, PH-TERM-AUDIT)
- `tool.terminal.shell.{open,input,close}` (persistent shells,
  PH-TERM-SHELL)
- `tool.terminal.shell.control` (named control chars,
  PH-TERM-CONTROL)

PH-TERM-PTY (D-010 closed): opt-in PTY backend behind
`--features terminal-pty`. Operators flipping
`[tool.terminal] pty = true` get a real pseudoterminal via
`portable-pty`; TUI / isatty-checking programs now work.
The default pipe-based path is unchanged. Loud-fail at
`ToolBackend::new` when `pty = true` is set without the
feature compiled.

Refactors landed:
- `validate_and_spawn` + `drive_to_completion` shared by run +
  spawn + shell.open (SpawnMode enum).
- `write_to_session_stdin` shared by shell.input + shell.control.
- Manual stdout/stderr drain replaced `wait_with_output`, enabling
  cancel + tail.
- `terminal.rs` → `terminal/` directory split with feature-gated
  `terminal/pty.rs`.

Remaining honest gaps (not Wave 1 blockers, deferred):
- Streaming-with-consumer-drain: tail is read-only; a > 1 MiB
  producer still stalls when the bounded buffer fills.
- Command-boundary tracking inside a shell session.
- PTY: stdout+stderr muxed on one stream (kernel limitation;
  PTY-mode rows have stderr empty); 80x24 fixed size, no
  SIGWINCH; ANSI escapes pass through `tail` verbatim.

### E. Web — markdown extraction shipped

`tool.web_fetch / web_get / web_search / web.robots_check`
already shipped. `tool.web_extract` now also supports
`markdown` mode (PH-WEB-MARKDOWN) — full HTML-to-Markdown
conversion (headings, paragraphs, links, lists, code blocks,
blockquotes, hr, emphasis, images). Hermes gaps remaining:
POST, cookies, URL safety (URLhaus), OSV check, crawl_limited,
explicit OpenGraph extraction (current meta mode handles it
indirectly). All decision-free.

### F. PDF / document — text chunking shipped

`tool.pdf` shipped. `tool.text.chunk` (PH-PDF-CHUNK) is the new
general text chunker — works on any text source (PDF extract,
HTML extract, web_get body, read_file). Paragraph > sentence >
word > char break priority; UTF-8 safe (char-counted).
Hermes gaps remaining: page-limited extraction, document
metadata beyond /Info dict (XMP). Decision-free.

### G. Safety / policy — descriptor surface only

Every shipped capability has sensitivity tags + requires_groups
+ idempotency + cost_class set honestly. The dispatch-side
sanitizer (`sanitize.rs`) covers tool-call repair. No
**`risk_level`** field on `CapabilityDescriptor` yet; adding it
would be a broad change touching every descriptor.

### H. Capability registry — current

Every new capability this session was added to the manifest in
`controller_runtime.rs`. `docs/capabilities.md` was kept in sync.
Dashboard explorer (PH-DASH3) was untouched but reads from the
manifest, so new capabilities appear automatically.

### I. Observability / dashboard — no movement this session

Existing dashboard surfaces (capability explorer, firehose,
provider health) still work. Per-tool-family inspectors (browser
sessions, MCP server explorer, terminal sessions panel, fs audit
panel) remain pending.

### J. Tests + docs — kept current per milestone

Every milestone shipped tests + updated `docs/capabilities.md` +
tallied into `docs/internal/recovered-execution-state.md`. The
Hermes capability map was refreshed (`docs/internal/hermes-capability-map.md`).

---

## Pending decisions (unblock real backends)

From `docs/internal/decisions-pending.md`:

| ID | Topic | Recommendation | Blocks |
|---|---|---|---|
| D-001 | task.memory adoption | (b) defer | memory_tool parity |
| D-002 | MCP trust tiers | (b) single-tier operator-opt-in | None (proceed) |
| D-003 | Chronicle compaction threshold | (c) on-demand only | H2 backfill |
| D-004 | `origin_surface` column | (a) ship — additive | None |
| D-006 | iteration-budget grace-call | (c) — **shipped** | — |
| D-007 | computer_use_tool backend | (c) defer | computer_use_tool |
| D-008 | Browser backend (PW vs HC vs WD) | multi-backend — **shipped all three behind features** | — |
| D-009 | MCP server target | **shipped** live stdio runtime (PH-MCP-RUNTIME) — operator supplies `command` + `args` per server | — |
| D-010 | PTY backend (`portable-pty`) | **shipped** behind `--features terminal-pty` (PH-TERM-PTY) | — |

All three Wave 1 blockers (D-008 / D-009 / D-010) closed.
The remaining decisions (D-001/2/3/4/6/7) are out of Wave 1
scope; they'd unlock Hermes-parity follow-ups (task.memory,
MCP trust tiers, chronicle compaction, etc.) but no Wave 1
capability surface depends on them.

---

## Capability counts (per `docs/capabilities.md`)

### Filesystem (`tool/fs.rs`)
- tool.read_file
- tool.write_file
- tool.append_file (PH-FS-PARITY1)
- tool.list_dir (CW2)
- tool.search_files (name/content/glob; PH-FS-PARITY3)
- tool.patch
- tool.patch_preview (PH-FS-PARITY1)
- tool.binary_sniff (PH-FS-PARITY2)
- tool.fs.audit_recent (PH-FS-PARITY4)
- tool.fuzzy_replace (PH-FS-FUZZY)
- tool.fs.tree (PH-FS-TREE)
- tool.fs.stat (PH-FS-STAT)

### Terminal (`tool/terminal.rs`)
- tool.terminal.run (CW1)
- tool.terminal.spawn (PH-TERM-SPAWN)
- tool.terminal.cancel (PH-TERM-CANCEL)
- tool.terminal.sessions (PH-TERM-SESSIONS)
- tool.terminal.tail (PH-TERM-STREAM1)
- tool.terminal.audit_recent (PH-TERM-AUDIT)
- tool.terminal.shell.open (PH-TERM-SHELL)
- tool.terminal.shell.input (PH-TERM-SHELL)
- tool.terminal.shell.close (PH-TERM-SHELL)
- tool.terminal.shell.control (PH-TERM-CONTROL)

### Browser (`tool/browser.rs`) — scaffold
- tool.browser.{open_session,close_session,navigate,get_text,
  screenshot,list_sessions} — all `BackendNotConnected` (D-008)

### MCP (`tool/mcp.rs`) — scaffold + protocol layer
- tool.mcp.{list_servers,list_tools,invoke}
- `mcp::proto` module: JsonRpcRequest/Response/Notification +
  ToolsListResult / ToolsCallResult + serialize_request /
  parse_response_line (PH-MCP-PROTO; D-009)

### Web (`tool/web_*.rs`)
- tool.web_fetch / tool.web_get / tool.web_search /
  tool.web_extract / tool.web.robots_check

### PDF
- tool.pdf

---

## Highest-leverage next moves (decision-free)

1. **PH-WEB-MARKDOWN** — `tool.web.extract_markdown`. Convert
   HTML to Markdown via a small hand-rolled pass or
   `pulldown-cmark` (already a transitive dep candidate; verify
   before adding). Decision-free.

2. **PH-WEB-METADATA** — `tool.web.extract_metadata`. OpenGraph
   + Twitter Card + standard `<meta>` extraction. Decision-free.

3. **PH-FS-METADATA-RING** — extend `tool.fs.audit_recent` with
   a `?op=` filter so operators can pull only writes / only
   appends. Trivial.

4. **PH-CAP-RISK** — add `risk_level` field to
   `CapabilityDescriptor` and audit every existing descriptor
   to set it. Broad change but mechanical; touches every tool
   module + the manifest exposure.

5. **PH-PDF-CHUNK** — `tool.pdf.chunk_text`. Splits a PDF's
   extracted text into bounded chunks. Decision-free.

6. **PH-MCP-CLI** — `relix-cli ops mcp` subcommand that mirrors
   `tool.mcp.list_servers / list_tools`. Pure CLI add. Pairs
   well with the existing `ops capabilities` / `ops events` /
   `ops route-test` / `ops providers-health` family.

7. **PH-TERM-CLI** — `relix-cli ops terminal-sessions /
   terminal-audit`. Same shape.

Recommend taking 2-3 in a single session, all decision-free.

---

## Exact commands to resume

```bash
cd D:/DATA/WORK/OpenPrem/Apps/Relix

# Confirm clean state
git status
git log --oneline -10

# Re-run full gates to confirm nothing rotted while paused
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# Expected: 995 tests passing, 0 failures.

# Read the recovery state to understand what shipped this session
cat docs/internal/recovered-execution-state.md

# Pick the next milestone. Recommended order if user hasn't
# answered D-008 / D-009:
#   1. PH-WEB-MARKDOWN  (web extraction parity, no new dep)
#   2. PH-PDF-CHUNK     (document chunking parity)
#   3. PH-MCP-CLI       (CLI surface for MCP registry)
#   4. PH-TERM-CLI      (CLI surface for terminal sessions/audit)
#   5. PH-CAP-RISK      (broad: risk_level on descriptors)
```

---

## Honesty contract (preserved across the session)

1. **No fabricated graph edges** — only real producer events.
2. **No fake hard-preemption** — cooperative cancel only.
3. **No fake provider failover** — operator-visible state with
   restart-required messaging.
4. **No fake backend success** — `tool.browser.*` returns
   `BackendNotConnected`; `tool.mcp.invoke` returns
   `RuntimeNotConnected`. Both refuse to fake.
5. **Append-only chronicle** — capabilities don't write
   chronicle directly. Audit rings (PH-FS-PARITY4,
   PH-TERM-AUDIT) are process-local observability surfaces.
6. **Honest limitations documented** — every capability with a
   gap (no PTY, no streaming-with-drain, no command-boundary
   tracking) carries that limitation in its module doc + the
   relevant tracking decision (D-008/9/10).

---

## Architectural warnings / things to watch

1. **`tool.terminal.shell.*` uses pipe stdin, not PTY.** Programs
   that check `isatty()` see false. Programs that rely on TTY-
   driven signal translation (interactive bash converting `0x03`
   into SIGINT for the foreground job) do not see the signals.
   D-010 logged; recommendation is to defer until a concrete
   isatty-blocked workflow appears.

2. **`tool.terminal.tail` is read-only.** It does NOT advance
   the drainer's write head, so a > 1 MiB producer still stalls
   when the bounded buffer fills. A future ring-with-consumer-
   cursor would relax this.

3. **`tool.fuzzy_replace` refuses on multi-match.** Operators
   who need to disambiguate must rephrase the search block
   with more surrounding context. This is intentional — no
   automatic disambiguation.

4. **`SpawnedRun.stdin` is only Some in shell mode.** The
   `validate_and_spawn` function branches on `SpawnMode`
   (Run / Shell). When adding new spawn modes (e.g., Pty),
   extend the enum and the stdio configuration branch
   alongside.

5. **`backend.allowed_shells` is empty by default.** Operators
   must explicitly list shells in `[tool.terminal] allowed_shells`
   for `shell.open` to succeed. Backend construction now
   requires at least one of `allowed_commands` / `allowed_shells`
   to be non-empty (was: only `allowed_commands` required).

---

## WHEN USER SAYS CONTINUE, START HERE

```
1. cd D:/DATA/WORK/OpenPrem/Apps/Relix
2. git pull --ff-only origin main
3. git status   (confirm clean)
4. cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
5. cargo test --workspace
   (Expected: 995 tests, 0 failures)
6. Read THIS file (docs/internal/continuation-state.md) for full context.
7. Read docs/internal/recovered-execution-state.md for the recovery snapshot.
8. Read docs/internal/decisions-pending.md — operator decisions D-001..D-010.
9. Read docs/internal/hermes-capability-map.md — Hermes parity status.
10. Read docs/capabilities.md — canonical capability index.
11. Pick the next milestone. If the user has answered D-008 / D-009,
    take the unblocked backend track first. Otherwise pick from the
    "Highest-leverage next moves" list above.
12. Maintain the honesty contract (above).
13. After each milestone:
    - cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
    - cargo test --workspace
    - git add <files> && git commit -m "..." && git push origin main
    - Tally into docs/internal/recovered-execution-state.md.
14. NO Claude attribution in commits. NO co-author trailers.
15. NO hardcoded provider tokens. NO secrets in committed files.
16. If a decision is genuinely needed and you can't act safely:
    - log to docs/internal/decisions-pending.md
    - pivot to another track
    - never block the night on a question the user can answer tomorrow.
```
