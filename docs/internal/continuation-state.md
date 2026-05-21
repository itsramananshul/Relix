# Continuation State — Claude Session Handoff

This file is a single-source checkpoint written before a usage-limit
pause. The next Claude session should treat the **WHEN USER SAYS
CONTINUE, START HERE** block at the end as the authoritative
resume command.

---

## Repository state

- **Branch:** `main`
- **Latest commit:** `2839df0c7ea0deb747048564ae4d9eb427ed0aed`
  - `feat(tool): CW1 terminal_tool capability foundation (sandboxed shell)`
- **`git status`:** clean working tree, branch up to date with `origin/main`
- **Remote:** `origin` → `https://github.com/itsramananshul/Relix.git`
- **Pushed:** yes, CW1 commit included
- **Modified files:** none (all committed)
- **Untracked files:** none material
  - `configs/policies/local.toml` was already untracked at session start
    (per the initial `gitStatus` snapshot); not session-introduced

## Workspace test posture at the checkpoint

Last full `cargo test --workspace` run was clean **after** the M76 +
CW1 commits:

```
relix-core      54 passed
relix-policy    33 passed
relix-runtime  291 passed
relix-telegram  23 passed
relix-cli        2 passed
relix-web-bridge 192 passed
bridge integration tests 3 passed
all crate tests + doctests 0 failures total
```

`cargo clippy --workspace --all-targets -- -D warnings` was clean
immediately before the checkpoint commit. `cargo fmt --all` ran
clean.

## What completed in this session

Every milestone below shipped real persistent state / real new
capabilities / real behavioral changes. No fabricated graphs, no
fake hard-preemption claims, no synthesized edges. Honesty
contracts preserved per the strict invariants the user has
emphasized across rotations.

### Track 1 — Runtime Interruption

- **M70** intent/ack split + generation counters:
  - Schema migration: `pause_generation`, `freeze_generation`
    columns on `tasks` (always-incrementing per axis).
  - Renamed events: `task.paused` → `task.pause_requested`,
    `task.resumed` → `task.resume_requested` (request side).
  - New ack event types reserved: `task.pause_observed`,
    `task.resume_observed`, `task.freeze_propagated`.
  - New capabilities: `task.interruption_check` (cooperative-poller
    snapshot), `task.observe_interruption` (runtime ack emit).
  - 6 new unit tests; legacy chronicle lookup falls back to
    `task.paused` for pre-M70 paused tasks.
- **M71** `task.freeze` + `task.unfreeze` workflow-level
  interruption:
  - Schema migration: `frozen_at`, `frozen_reason` columns.
  - New `FREEZABLE_STATUSES` const wider than pause.
  - `set_frozen` / `set_unfrozen` mutators bump
    `freeze_generation`, emit `task.freeze_requested` /
    `task.unfreeze_requested` chronicle events.
  - Bridge endpoints `POST /v1/tasks/:id/freeze` +
    `/unfreeze`; dashboard actions next to Pause/Resume.
  - 5 new unit tests including pause/freeze generation
    independence.
- **M74** state-machine matrix:
  - `TASK_STATES` const + `is_allowed_transition(from, to)`
    helper documenting every permitted move.
  - `task.transition_check` capability is informational only —
    `task.update` not yet enforced against the matrix (honest
    scope; deferred to future milestone).
  - 5 unit tests (canonical happy path, disallowed pairs,
    same-status no-op, unknown-status forward-compat, store
    integration).
- **M76** retry suppression when paused/frozen:
  - `request_retry` now explicitly detects paused/frozen and
    emits `task.retry_suppressed` chronicle event with
    payload_json `{suppressed_by, retry_count, budget}`.
  - 2 new unit tests.

### Track 2 — Execution Graph Producers

- **M72** real cross-task edge producers:
  - 3 new coord capabilities: `task.record_spawned`,
    `task.record_delegated`, `task.record_awaited`.
  - Each writes the edge + a parent chronicle event in one
    transaction; `spawned_by_event_id` points at the event
    for dashboard click-through.
  - `EdgeProducerOutcome` struct carries `edge_id` + `event_id`.
  - Refuses self-edges, NotFound on unknown task_ids.
  - Optional metadata (`branch_id`, `context_id`, `reason`)
    omitted from payload when unset — no fabricated defaults.
  - 6 new unit tests + 1 lineage integration test confirming
    M66 walker picks up the new cross-task edges.

### Track 3 — Global Execution Firehose

- **M73** firehose SSE + drop accounting + dashboard upgrade:
  - New endpoint `GET /v1/tasks/events/stream` — long-lived
    SSE, 750ms poll interval, `STREAM_PAGE_LIMIT=500`.
  - Per-page drop accounting emits `dropped` SSE frame when
    a page hits the limit (with `next_cursor` + recovery note
    pointing at `/recent`).
  - Cursor recovery via `?since=N`, event-type filter via
    `?event_type=foo`.
  - Stream metrics register against synthetic `__firehose__`
    task_id so `/v1/streams` counts firehose consumers.
  - Dashboard upgraded: `openGlobalFirehoseStream()` opens
    `EventSource` on first overview tick; `dropped` frames
    raise warn toasts; status footer reports `SSE live` vs
    `polling` + cumulative drop count.

### Capability Wave (new track this session)

- **CW1** terminal_tool capability foundation:
  - New module `crates/relix-runtime/src/nodes/tool/terminal.rs`.
  - Opt-in via `[tool.terminal]` config section with required
    `allowed_commands` allowlist.
  - 8-layer fail-closed security model (documented in module
    docstring): opt-in registration, allowlist enforcement,
    no shell, path-traversal-free command resolution, hard
    timeout, output caps (1 MiB stdout + stderr each),
    no-env-inheritance default, optional working_dir.
  - Wire shape: JSON request `{command, args, timeout_secs?}`
    → JSON response `{exit_code, stdout, stderr,
    duration_ms, timed_out, truncated_stdout,
    truncated_stderr, command, timeout_secs}`.
  - Manifest descriptor `tool.terminal.run` with sensitivity
    tags `shell:execute / host:local / destructive:potential`.
  - Only advertised when config validates (allowlist non-empty
    + no path separators + non-zero timeout) — no phantom
    capabilities.
  - 7 unit tests covering construction validation +
    output-truncation semantics.
  - Wired into existing `tool::register` alongside
    fs/pdf/web_extract — co-located per Explore-agent
    survey recommendation.

## What was in progress when stopped

**Nothing partially edited at the source level.** The terminal.rs
file landed in a coherent, buildable, tested state immediately
before the stop signal. The 5 mechanical `ErrorEnvelope::new(...)`
→ struct-literal fixes that the initial drop required were applied
before the commit; no half-written hunks remain.

The active in-progress task (#72 `CW1 capability runtime
foundation + terminal_tool`) is **complete** at the foundation
level. Streaming / interruption / background execution were
explicitly scoped out as Gate 2 work — that's not "incomplete"
in this rotation's sense, that's "future milestone with the
deferral documented in the module docstring."

## Unfinished tasks (queued for the next rotation)

The TaskList is the authoritative reference. As of the
checkpoint:

**Pending (queued, not started this session):**
- #67 — M75 lineage subtree metrics (Track 2)
- #69 — M77 provider routing trace foundation (Track 4)
- #70 — M78 production hardening docs (Track 7)
- #71 — M79 dashboard density pass (Track 6)
- #73 — CW2 file_tools capability family
- #74 — CW3 web_tools capability family
- #75 — CW4 browser_tool capability foundation
- #76 — CW5 mcp_tool registration + runtime projection

**Completed this session (status tracking):**
- #62 M70, #63 M71, #64 M72, #65 M73, #66 M74, #68 M76, #72 CW1

**Already completed pre-session (preserved for context):**
- #48, #49, #51, #52, #53, #54, #55, #56, #57, #58, #59, #60, #61

**Tracker housekeeping note:** task #50 ("Continue rotation: Track
B → C → A") was set to in_progress in a much earlier rotation and
never closed — it's a meta-task and should be marked completed by
the next session (low priority).

## Roadmap queue across all 7 active tracks

The user has emphasized continuous parallel work across these
tracks. The next rotation should rotate through them rather
than depth-first one track.

### A. Real Execution Orchestration (interruption / state machine)
- Wire `task.update` path against the M74 transition matrix
  (audit every existing caller; this is the deferred enforcement)
- Cooperative-checkpoint helper API on the runtime side that
  reads `task.interruption_check` + emits
  `task.observe_interruption` automatically — runtime workers
  shouldn't have to remember the protocol
- Cancel propagation groundwork (parallel to freeze
  propagation; needs M72 cross-task edges to actually
  propagate)

### B. Execution Graph + Lineage
- **M75** subtree metrics (queued task #67): `task.subtree_metrics`
  capability aggregating wall clock + attempt count over the
  BFS subtree; dashboard subtree panel
- Surface the new M72 edge types in the dashboard exec-graph
  card (the renderer currently labels all reserved types as
  "no producer yet" — needs an update so spawned/delegated/
  awaited render as real edges when present)
- `resumed_from` + `parallel_branch` + `blocked_on` producer
  capabilities (mirror the M72 pattern for the three remaining
  reserved edge types)

### C. Global Event Firehose
- Server-side filter set support (currently only `event_type`
  exact-match; the user asked for filters by task/node/type)
- `task.event_replay` capability — replay a window from
  the chronicle ledger (the cursor only walks forward today)
- Lag metrics on the streaming endpoint
- Dashboard event inspector pane (click an event in the
  firehose → open per-event detail with payload_json
  formatted)

### D. Provider Runtime
- **M77** provider routing trace foundation (queued task #69):
  per-provider failed_request_count + last_failure_at +
  last_routing_decision in bridge secrets; test_provider
  increments; dashboard provider timeline panel
- Circuit-breaker state machine on top of M69 cooldown
- Live failover chain visibility (which provider would route
  next, given current quarantine + cooldown state)

### E. Capability Wave
- **CW2** file_tools (queued task #73): the tool node
  already has `fs::FsJail` — wire a similar opt-in module
  pattern; add list/search/patch/binary-detection beyond
  what fs.rs already does. Path-traversal protection lives
  in fs.rs; mirror it.
- **CW3** web_tools (queued task #74): `tool.web_fetch`
  already exists with SSRF protection. Add `web.search`
  (provider-routed), `web.extract` (already exists as
  `tool.web_extract`), `web.crawl` foundations. Reuse
  `security::resolve_safe_url` for all new endpoints.
- **CW4** browser_tool (queued task #75): substantial scope.
  Browser session lifecycle, CDP foundations. Likely a new
  node type (`browser`) rather than co-located on tool node.
- **CW5** mcp_tool (queued task #76): strategically massive
  per the user. MCP server registration, stdio + HTTP
  transport, capability discovery, runtime projection as
  native Relix capabilities. Probably its own crate
  (`crates/relix-mcp/`).

### F. Dashboard / Operator Console
- **M79** density pass (queued task #71): compact tables,
  keyboard navigation (j/k between tasks, `/` to focus
  search, `?` for help), denser detail layout, less card
  chrome. Reference: `references/OpenClaw`.
- Topology explorer expansion (current `/topology` page is
  a good base — needs per-peer panels for the capabilities
  the peer advertises)
- Live event rail (separate from firehose pane — narrower,
  always-visible sidebar fed by same SSE)

### G. Production Hardening
- **M78** production docs (queued task #70):
  - `docs/deployment.md` production checklist
  - Dashboard local-only warning surface
  - Auth/RBAC stub interfaces (real impl deferred)
  - Secret-storage + backup-restore + audit-log docs
  - Unsafe-exposure warnings
- Reverse-proxy guidance (the bridge has no HTTP auth in
  alpha — every config endpoint says so honestly; the docs
  should consolidate)

## Architectural warnings / things to watch

1. **`task.update` is NOT enforced against the M74 transition
   matrix.** This is deliberate scope. Every existing caller
   (8 tests + 4 bridge handlers) needs an audit before
   enforcement can land. Until then, the matrix is a
   pre-flight reference operators query via
   `task.transition_check`.

2. **Cooperative interruption is not actually polled by any
   runtime worker today.** M70/M71 ship the read/write
   protocol; the SOL VM is synchronous and doesn't yet check
   `interruption_check`. The chronicle accurately records this:
   a `task.pause_requested` with no matching
   `task.pause_observed` means the runtime never noticed.
   This is the honest design.

3. **Provider quarantine (M69) is enforced only at the
   test-provider endpoint.** The AI controller does NOT
   live-read provider state — restart required for routing
   changes. The response `note` field surfaces this
   gap on every quarantine mutation. M77 (when shipped) will
   add the routing-trace observation layer; live enforcement
   needs an AI-controller hot-reload primitive that doesn't
   exist yet.

4. **Capability handlers do NOT write to task chronicle
   directly.** Per the existing pattern (verified by Explore
   agent): tool handlers return `HandlerOutcome::Ok | Err`
   and rely on the dispatch-side audit log. The chronicle is
   coord-side only. If/when capabilities should attest
   themselves into a parent task's chronicle, a new field
   needs adding to `InvocationCtx`. Deferred to Gate 2.

5. **The terminal capability does NOT have process
   interruption mid-run.** Hard timeout is the only stop.
   Documented in module docstring as explicit out-of-scope.

6. **CW1 unit tests use `Result::unwrap_err` which required
   `Debug` on `TerminalBackend`.** Added the derive. If
   anyone refactors the backend, keep the derive — tests
   depend on it.

7. **CW1 manifest descriptor is only advertised when the
   allowlist validates** (lines ~670 of controller_runtime.rs
   check `terminal::TerminalBackend::new(...).is_ok()`).
   This means the manifest matches what was actually
   registered — no phantom capabilities — but it also means
   a misconfigured `[tool.terminal]` results in BOTH the
   capability missing AND a warn log at startup. The warn
   log is the operator-facing diagnostic.

## Exact commands to resume

```bash
# Verify clean state
git status
git log --oneline -10

# Re-run the full test suite to confirm nothing rotted while paused
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Resume by picking the next pending task. Recommended order:
# 1. M75 subtree metrics (Track 2 — small, tightly bound to M66/M72)
# 2. M77 provider routing trace foundation (Track 4)
# 3. CW2 file_tools (extends existing fs.rs)
# 4. M78 production docs (parallel-safe; no code conflict)
# Avoid: CW5 mcp_tool until a real MCP server target is identified.
```

## Tests not run during the checkpoint

None — full workspace test pass succeeded immediately before
the commit. Nothing skipped.

## Errors encountered during this rotation

1. **`ErrorEnvelope::new(...)` doesn't exist.** Fixed by
   converting to struct literal (5 call sites in
   terminal.rs). The existing `tool/mod.rs` callers all use
   struct literals — I should have mirrored that pattern
   immediately.

2. **`iter().cloned().collect::<Vec<_>>().join(...)`
   triggered `clippy::iter_cloned_collect`** since
   `allowed_commands` is already `Vec<String>` (just call
   `.join(", ")` directly). Fixed.

3. **Doc list overindentation in `is_allowed_transition`
   docstring** flagged by clippy. Fixed by aligning
   continuation lines to the same indent.

4. **TaskCreate task #73 was accidentally set to in_progress
   before CW1 was actually started.** Reverted to pending,
   set #72 to in_progress instead. Tracker is correct now.

No source-level errors remain. No compilation warnings. No
test failures.

---

## WHEN USER SAYS CONTINUE, START HERE

```
1. cd D:\DATA\WORK\OpenPrem\Apps\Relix
2. git pull --ff-only origin main
3. git status   (confirm clean)
4. cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
5. cargo test --workspace
   (Expected: 291 runtime + 192 bridge + others, 0 failures)
6. Read THIS file (docs/internal/continuation-state.md) for full context.
7. Read TaskList; pick the next pending milestone. Recommended order:
   a. M75 (lineage subtree metrics) — small, tightly bound to M66/M72,
      good warmup that exercises the test suite after restart.
   b. M77 (provider routing trace foundation) — Track 4 has been quiet
      since M69; the user emphasized provider routing in the latest
      directive.
   c. CW2 (file_tools) — extends existing fs.rs pattern, low-risk
      capability wave continuation.
   d. M78 (production hardening docs) — code-conflict-free; can run
      in parallel mentally.
8. Maintain the HONESTY CONTRACT:
   - No fabricated graph edges (real producers only).
   - No fake hard-preemption (cooperative only; intent vs ack split).
   - No fake provider failover (operator-visible state + restart-
     required messaging).
   - "(not recorded yet)" / "(not emitted yet)" labels stay honest
     where data is missing.
9. After each milestone:
   - cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
   - cargo test --workspace
   - git add <files> && git commit -m "..." && git push origin main
10. DO NOT batch multiple milestones into one commit.
11. DO NOT abandon current track work to start the Capability Wave;
    rotate through tracks as the user has reaffirmed.
12. Open issues to watch:
    - task.update enforcement vs M74 matrix (deferred audit)
    - CW1 has no process-interruption (Gate 2 scope)
    - AI controller doesn't live-read provider quarantine
      (M77 should NOT silently fix this; it remains an honest gap)
```

---

Generated at the request of the user when their Claude usage
limit was about to exhaust. Treat this file as the source of
truth for resume — do not require any session-context recovery
beyond reading it.
