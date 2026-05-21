# Recovered Execution State — Post-Shutdown Reconstruction

**Recovered at:** 2026-05-21 (post hard shutdown).
**Source of truth:** repository state itself (commits, code, internal docs).
**Method:** read `docs/internal/continuation-state.md`, `decisions-pending.md`,
`hermes-capability-map.md`, `docs/router-node-impl-prompt.md`,
`git log --oneline -25`, and a targeted survey of
`crates/relix-runtime/src/nodes/tool/{browser,mcp,fs,terminal}.rs`.

> This document is a snapshot, not a plan. The plan is in
> `continuation-state.md` (queue) + this file's *Next Execution
> Priorities* section.

---

## 1. Repository State at Recovery

- **Branch:** `main`
- **HEAD:** `3771a3a docs(internal): log PH-ROUTER-NODE in resume tally`
- **Status:** working tree has only `docs/internal/decisions-pending.md`
  modified plus two untracked docs (`docs/relix-summary-for-chai.md`,
  `docs/router-node-impl-prompt.md`). No source changes pending.
- **Remote:** up to date with `origin/main`.
- **Workspace tests as of 2026-05-21 (continuation snapshot):**
  ~796 passing, 0 failures across relix-core (61), relix-policy (33),
  relix-runtime (497), relix-telegram (23), relix-cli (61),
  relix-web-bridge (220), plus 3 bridge integration tests.

---

## 2. Architecture — Active Subsystems

### 2.1 Controller binary roles
- `relix-controller` is the one binary. Role differentiated by config:
  - `role = "controller"` (default) — runs all the AI/tool/coord nodes.
  - `role = "router"` (shipped via PH-ROUTER-NODE / commit 689f819) —
    mesh observability + health control plane. Tracks peer
    heartbeats, sessions, aggregated logs. 4 caps:
    `router.{heartbeat,network_summary,session_list,log}`.
- Configs in `configs/router-node.toml` and `configs/policies/router.toml`.

### 2.2 Coordinator — task lifecycle + chronicle
- SQLite-backed `tasks` table with M70/M71 schema additions:
  `pause_generation`, `freeze_generation`, `frozen_at`, `frozen_reason`.
- Append-only chronicle ledger; intent vs ack split for pause / resume
  (M70) and freeze / unfreeze (M71).
- State-machine matrix (M74) — `is_allowed_transition` reference helper.
  **`task.update` is NOT enforced against the matrix yet** — explicit
  deferred audit.
- Recovery scan auto-emits synthesized `task.terminal_summary` events
  on flip-to-interrupted (D-006 closure).
- Edge producers: `task.record_spawned|delegated|awaited` (M72).
  Three other reserved edge types (`resumed_from`, `parallel_branch`,
  `blocked_on`) still need producer capabilities.

### 2.3 Provider runtime
- **HealthAwareRouter** (PH-ROUTER2 / 41ee3bb): health-aware provider
  picker. Preview endpoint at `POST /v1/providers/route_test`.
  CLI: `relix-cli ops route-test`. **Operator-facing observability
  only — the AI node's live request path still uses the
  single-provider posture.** No live cross-provider failover yet.
- Per-provider `failed_request_count` / `last_failure_at` /
  `last_routing_decision` recorded (M77 work in this area is open).
- Rate-limit ladder (PH-WAVE2 G..L): per-provider observation ring,
  auto-cooldown trigger, dashboard banner, consolidated
  `/v1/providers/health` endpoint, `relix-cli ops providers-health`.

### 2.4 Bridge / firehose / dashboard
- Global firehose SSE at `GET /v1/tasks/events/stream` (M73) with
  cursor recovery + drop accounting + dashboard auto-connect.
- `relix-cli ops events` snapshot (PH-OPS-EVENTS).
- `#/capabilities` dashboard explorer page (PH-DASH3) + CLI subcommand.
- Per-event firehose row click-to-expand (PH-DASH1).
- Bridge route latency tracing middleware (H15).

### 2.5 Honesty / safety scrubber
- Secret-redaction in chronicle + audit (H8 / H9 / H10) covers every
  operator/provider-supplied free-text boundary. PH-WAVE2D extended
  to `task.todo_set`.
- `sanitize.rs` (PH5): JSON arg repair/coerce/truncate utilities.
- Jittered backoff helper (PH-WAVE2A).

---

## 3. Tool Capabilities Inventory

Authoritative index: `docs/capabilities.md`. Status by family:

### 3.1 Filesystem (`crates/relix-runtime/src/nodes/tool/fs.rs`)
- `tool.read_file`, `tool.write_file`, `tool.list_dir` — shipped.
- `tool.search_files` — shipped. **Substring-only name+content match;
  no glob, no regex.** Linear walker (`walk_under`, 50K cap).
- `tool.patch` — shipped (diffy unified-diff).
- `tool.append_file` + `tool.patch_preview` — shipped (PH-FS-PARITY1).
- `tool.binary_sniff` — shipped (PH-FS-PARITY2).
- **Gaps vs Hermes parity:** glob/regex search mode, recursive +
  pattern combined, fuzzy_replace, mutation audit events (today the
  fs handlers don't emit chronicle/audit events — relies on
  dispatch-side audit log only).

### 3.2 Terminal (`tool/terminal.rs` — 431 LOC)
- `tool.terminal.run` shipped (CW1). 8-layer fail-closed model:
  opt-in, allowlist, no shell, path-traversal-free resolve, hard
  timeout, 1 MiB stdout/stderr caps, no-env-inherit, optional cwd.
- **Gaps:** no process sessions; no streaming; no cancel mid-run
  (hard timeout is the only stop — Gate 2 deferred); no background
  process tracking; no command history; no per-process audit events
  beyond what dispatch records.

### 3.3 Browser (`tool/browser.rs` — 722 LOC, scaffold)
- `tool.browser.{open_session,close_session,navigate,get_text,
  screenshot,list_sessions}` — capability surface wired; `NoneBackend`
  is the only impl. `BackendNotConnected` on every non-noop.
- Real backend choice **BLOCKED on D-008** (Playwright sidecar vs
  headless_chrome vs WebDriver). Recommendation: (b) headless_chrome.
- Scaffold honesty contract holds: ids allocate but nothing drives a
  real browser; chronicle never sees fake nav events.

### 3.4 MCP (`tool/mcp.rs` — 540 LOC, scaffold)
- `tool.mcp.{list_servers,list_tools,invoke}` — registry + discovery
  shipped. `[[tool.mcp.servers]]` config entries declare servers.
- **Gaps:** no live stdio or HTTP client. `tool.mcp.invoke` returns
  typed `RuntimeNotConnected` always. No initialize handshake. No
  reconnect.
- **D-002 is open** but recommendation is (b) single-tier
  operator-explicit opt-in — which is already the scaffold's posture.
  Treat this as safe to proceed with stdio runtime without operator
  sign-off; trust-tier classification is independent.

### 3.5 Web (`tool/web_tools.rs`, `web_extract.rs`, `web_robots.rs`)
- `tool.web_fetch` (SSRF-protected GET) — shipped.
- `tool.web_get`, `tool.web_search` (DuckDuckGo scrape) — shipped (CW3).
- `tool.web_extract` (DOM + CSS selectors, hand-rolled) — shipped.
- `tool.web.robots_check` (RFC 9309 robots.txt sniff) — shipped
  (PH-WEB-ROBOTS).
- **Gaps vs Hermes:** POST/cookies, `url_safety` (phishing checks),
  `osv_check`, vision/transcription/feishu families — all pending.

### 3.6 PDF — `tool.pdf_extract` shipped.

### 3.7 Coord-side capabilities
- `task.todo_{set,list,update}` (PH-WAVE2D)
- `task.{spawned,delegated,awaited}` edge producers (M72)
- `task.{pause,resume,freeze,unfreeze}_requested/observed` (M70/M71)
- `task.interruption_check`, `task.observe_interruption` (M70)
- `task.transition_check` (M74)
- `task.recent_events` + chronicle helpers

---

## 4. Hermes Parity Snapshot

From `hermes-capability-map.md` (76 tools / 35 skills / 11+ platforms
inventoried). Rough parity estimate by tool family:

| Family | Hermes # | Relix shipped | Partial | Pending |
|---|---|---|---|---|
| Filesystem | 7 | 5 | 2 | 0 |
| Web / network | 8 | 3 | 1 | 4 |
| Browser | 7 | 0 (scaffold) | 1 (CW4) | 6 |
| Terminal / exec | 5 | 2 | 1 | 2 |
| Memory / context | 4 | 0 | 1 | 3 |
| Planning / orchestration | 4 | 1 (todo_tool) | 1 (delegate) | 2 |
| Platforms / messaging | 6 | 1 (telegram) | 1 | 4 |
| Model / inference | 5 | 0 | 0 | 5 |

Aggregate: ~14 shipped + ~8 partial vs 46 Hermes tools = ~30% parity
by tool count, but the shipped tools are the high-leverage ones
(file ops, terminal, web, todo, edge producers).

The 7 H-cluster Hermes adaptations (failover reason, chronicle
summarizer, anti-thrash, terminal_summary, stuck-running, orphan-cleanup,
redaction) all shipped tonight as ports of Hermes patterns onto
Relix's chronicle / coordinator instead of as new tools.

---

## 5. Pending Decisions Blocking Work

From `docs/internal/decisions-pending.md`:

| ID | Topic | Recommendation | Blocks |
|---|---|---|---|
| D-001 | task.memory adoption | (b) defer — needs AI-node context engine | memory_tool parity |
| D-002 | MCP trust tiers | (b) single-tier operator-opt-in | None — proceed |
| D-003 | Chronicle compaction threshold | (c) on-demand only | H2 backfill |
| D-004 | `origin_surface` column | (a) ship — additive | None — proceed |
| D-006 | iteration-budget grace-call | (c) recovery-scan synth — **shipped** | — |
| D-007 | computer_use_tool backend | (c) defer | computer_use_tool |
| D-008 | Browser backend (PW vs HC vs WD) | (b) headless_chrome | **CW4 real backend** |

**Operator must answer:** D-001, D-003, D-007, **D-008** before
those tracks land real backends.

---

## 6. Tracks — Where Each Stands

### A. Real Execution Orchestration
- `task.update` enforcement vs M74 matrix — **deferred audit**.
- Cooperative-checkpoint helper API — runtime workers don't poll yet.
- Cancel propagation — needs M72 edges + a propagator.

### B. Execution Graph + Lineage
- M75 subtree metrics — **pending**.
- Surface M72 edge types in dashboard exec-graph card — pending.
- `resumed_from` / `parallel_branch` / `blocked_on` producers — pending.

### C. Global Event Firehose
- Server-side multi-filter set — pending (only `event_type` today).
- `task.event_replay` — pending.
- Lag metrics — pending.
- Dashboard event inspector pane — pending.

### D. Provider Runtime
- M77 routing-trace foundation — partial (per-provider failure
  counters exist; live AI controller hot-reload missing).
- Circuit-breaker state machine on top of M69 cooldown — pending.
- Live failover chain visibility — partial via HealthAwareRouter
  preview; not wired into AI node's live request path.

### E. Capability Wave (Wave 1 backend conversion — **CURRENT WAVE**)
- CW1 terminal: shipped foundation; **needs sessions/streaming/cancel**.
- CW2 file_tools: shipped read/write/search/patch/append/preview/sniff;
  **needs glob/regex + mutation audit events**.
- CW3 web_tools: shipped GET/search/extract/robots; needs POST.
- CW4 browser: scaffold only; **D-008 blocked**.
- CW5 mcp: scaffold only; **needs real stdio runtime + projection**.

### F. Dashboard / Operator Console
- M79 density pass — pending.
- Topology explorer per-peer panels — partial.
- Live event rail (separate from firehose) — pending.

### G. Production Hardening
- M78 production docs — pending.
- Reverse-proxy guidance — pending.

---

## 7. Important Invariants (DO NOT VIOLATE)

1. The responding node enforces identity → policy → handler → audit.
2. AI provider keys live ONLY in the AI node's local config.
3. The web backend in `RELIX_MODE` makes zero LLM provider calls.
4. No routing decision outside SOL flows.
5. Adding a new channel node requires zero changes to memory / AI /
   tool / web nodes.
6. **Honesty contract**: no fabricated graph edges, no fake
   hard-preemption, no fake provider failover. `(not recorded yet)` /
   `(not emitted yet)` labels stay honest where data is missing.
7. **Append-only chronicle**: capabilities don't mutate chronicle
   rows; H2 / H6 / H7 produce *new* events, not edits.
8. Router node NEVER makes LLM calls and NEVER holds provider keys.

---

## 8. Next Execution Priorities

Wave 1 conversion — real backends — in the order I'll attempt them.
Each must land green (fmt + clippy + tests) before the next starts.

1. **PH-FS-PARITY3 — glob search + mutation audit events**
   Recursive walker exists. Add `glob` mode using `globset` or
   hand-roll, plus chronicle audit events on `write_file` / `append_file`
   / `patch`. Pure additive. Decision-free.

2. **CW5-A — MCP stdio runtime (initialize + tools/list + tools/call)**
   D-002 recommendation (b) aligns with current scaffold posture.
   Implement subprocess spawn, JSON-RPC handshake, projection cache.
   `RuntimeNotConnected` becomes a *real* state — only fires when the
   server failed to start or speak protocol.

3. **CW1-A — Terminal hardening (sessions + cancel + background)**
   Process sessions, streaming, cancellation. Honesty: hard timeout
   stays as the safety floor; cancel is cooperative on top.

4. **CW4 deferred** until D-008 answered. If unblocked,
   `headless_chrome` backend is the recommended path.

If any of (1)-(3) blocks on something I cannot safely resolve, log
the blocker to `docs/internal/decisions-pending.md` and pivot to the
next item.

---

## 9. Quality Gates (Repeat Every Milestone)

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Then commit with `feat(<scope>): ...` style — no Claude attribution,
no co-author trailers — and push to `origin/main`.

---

## 10. What Recovery Did NOT Change

- No source files edited.
- No tests modified.
- No git operations beyond `status` / `log` reads.
- The recovery doc itself is the only new artifact.

---

## 11. Recovery-Session Wave 1 Progress

After writing this doc, the session shipped six commits on `main`:

| Commit | Milestone | Tests | Concept |
|---|---|---|---|
| ca08428 | docs(internal) | n/a | recovered-execution-state + D-008 |
| 8ed8e05 | PH-FS-PARITY3 | runtime +10 | tool.search_files `glob` mode (hand-rolled `*`/`**`/`?`) |
| 47b5db9 | PH-FS-PARITY4 | runtime +8 | tool.fs.audit_recent + per-jail mutation ring |
| 1feb8ce | PH-TERM-SESSIONS | runtime +6 | tool.terminal.sessions — live in-flight run registry |
| acac673 | PH-MCP-PROTO | runtime +12 | MCP JSON-RPC wire layer (no I/O) + D-009 logged |
| 512e727 | PH-TERM-AUDIT | runtime +8 | tool.terminal.audit_recent + per-backend completion ring |
| 9d4381d | PH-TERM-CANCEL | runtime +7 | tool.terminal.cancel + manual stdout/stderr drain refactor |
| c8d7fcb | PH-TERM-STREAM1 | runtime +11 | tool.terminal.tail — polling-cursor live stdout/stderr stream |
| d5587d2 | PH-TERM-SPAWN | runtime +6 | tool.terminal.spawn — fire-and-forget background variant; validate_and_spawn / drive_to_completion refactor |
| 57c575d | PH-TERM-SHELL | runtime +11 | tool.terminal.shell.{open,input,close} — persistent shell sessions; SpawnMode enum on validate_and_spawn |
| cf2ea48 | PH-TERM-CONTROL | runtime +10 | tool.terminal.shell.control — named control char writer (etx/eot/tab/enter/...); D-010 PTY decision logged |

Aggregate: ~3300+ LOC across tool/fs.rs, tool/terminal.rs, tool/mcp.rs,
controller_runtime.rs, docs/capabilities.md. Runtime tests
507 → 596 (+89). Workspace 887 → 976 passing. fmt + clippy clean
on every commit.

**Decisions logged but unanswered:** D-008 (browser backend),
D-009 (MCP server target). Both block real backend wiring until
operator picks; the surfaces themselves keep the honesty contract
(NoneBackend / RuntimeNotConnected).

**Wave 1 tracks still pending after this session:**
- Full PTY backend for `tool.terminal.shell.*` — D-010 blocked.
  Current shells use a pipe stdin, not a pseudo-terminal;
  isatty()-checking and TTY-signal-translating programs are
  not supported.
- Terminal streaming-with-consumer-drain (the current tail is
  read-only; a > 1 MiB producer still stalls when the buffer
  fills).
- Terminal command-boundary tracking inside a shell session
  (today the shell is a single bytes-in / bytes-out stream;
  per-command exit codes need operator-supplied sentinels).
- Browser real backend — D-008 blocked.
- MCP stdio runtime wiring — D-009 blocked.
- Filesystem fuzzy_replace — explicitly out-of-scope per fs.rs
  module docstring (alpha decision).

**Wave 1 tracks now complete (within the no-PTY model):**
- Terminal control characters — `tool.terminal.shell.control`
  named control writer (PH-TERM-CONTROL).
- Terminal persistent shell — `tool.terminal.shell.{open,input,
  close}` (PH-TERM-SHELL).
- Terminal background execution — `tool.terminal.spawn`
  fire-and-forget (PH-TERM-SPAWN).
- Terminal cancel — `tool.terminal.cancel` cooperative termination
  (PH-TERM-CANCEL).
- Terminal streaming (read-only tail) — `tool.terminal.tail`
  polling cursor (PH-TERM-STREAM1).
- Terminal process registry — `tool.terminal.sessions` (PH-TERM-SESSIONS).
- Terminal completion observability — `tool.terminal.audit_recent`
  (PH-TERM-AUDIT).
- Filesystem glob search — `tool.search_files` `glob` mode
  (PH-FS-PARITY3).
- Filesystem mutation audit — `tool.fs.audit_recent` (PH-FS-PARITY4).
- MCP wire layer — JSON-RPC types ready for the next session
  (PH-MCP-PROTO).

**Cross-cutting parity work that's now well-positioned:**
- The per-capability observability ring pattern is now established
  (fs.audit_recent + terminal.sessions + terminal.audit_recent).
  Future capabilities should ship a matching ring when they have
  state worth observing — and the MCP proto module is ready for
  the moment D-009 unblocks.
