# Decisions Pending — Operator Sign-off Required

This file collects fork-in-the-road questions encountered during
autonomous overnight work. The runtime did NOT make these decisions
on its own. Each entry is one option-set + recommendation; the user
answers in the morning and the runtime applies the chosen path.

The format intentionally keeps each entry short — the operator
should be able to skim and answer all of them in 5 minutes.

---

## Decision template

```
### D-NNN  <short title>

**Context.** One paragraph. Where does this come up; what is
already true in the repo.

**Options.**
- (a) ...
- (b) ...
- (c) skip / defer.

**Recommendation.** (one of a/b/c) — one sentence why.

**Status.** open / answered:<choice> / superseded.
```

---

## Open

### D-001  Hermes "memory char limits" — adopt for SOL flow checkpoints?

**Context.** Hermes uses char-based (not token-based) limits on
its memory store (2200 chars for MEMORY.md, 1375 for USER.md) so
the same limit works across providers. Relix has no analogous
per-flow memory store today; the SOL chronicle is per-event with
no global cap. A Hermes-style frozen-snapshot memory could land
as: a per-task `task.memory` capability that the AI node injects
into its own context, with a fixed char budget independent of the
backing provider.

**Options.**
- (a) Adopt the pattern as `task.memory.{read,write}` capabilities on
  the coordinator (SQLite-backed, fixed char cap, frozen snapshot
  per task generation). 2-3 days of work.
- (b) Defer until the AI node grows context-injection logic; today
  the AI node is mostly a thin provider shim and would need
  context-assembly machinery first.
- (c) Adopt only the *snapshot* concept (immutable per-turn view)
  without the storage layer; useful for replay determinism.

**Recommendation.** (b) — defer. The Hermes pattern shines because
Hermes owns its own LLM client and assembles every turn's prompt.
Relix's AI node delegates to the provider for prompt assembly.
Adding `task.memory` without a context-engine consumer would ship
write-side state with no read-side. Better to land the context
engine first, then frozen snapshots fall out of it naturally.

**Decision.** (b) defer. Confirmed at Wave 1 close — out of
scope. Revisit when the AI node grows context-injection
machinery; frozen snapshots fall out naturally then.

**Status.** answered: defer.

### D-002  Hermes "ClawHub community trust = never" — apply to MCP servers?

**Context.** Hermes hardcodes ClawHub at the community trust tier
because of an incident where 341 malicious skills were published.
Relix's CW5 plan adds MCP server registration via dashboard. The
question is whether to give MCP servers an explicit trust tier
(`builtin / trusted / community / agent-created`) like Hermes does,
or to treat every MCP server as a one-off operator-explicit
opt-in (today's posture for `tool.terminal.run`).

**Options.**
- (a) Adopt 4-tier trust matrix from Hermes (builtin, trusted,
  community, agent-created). Source URL controls the tier.
- (b) Single-tier "operator-explicit opt-in" — every MCP server
  the operator enables is treated as trusted; capability sensitivity
  tags continue to drive policy admission.
- (c) Two-tier (builtin vs operator-added) for now; expand later
  if MCP marketplace appears.

**Recommendation.** (b) — the existing policy engine already does
fine-grained capability admission via sensitivity tags; layering
trust tiers on top duplicates that. Hermes needs tiers because it
auto-installs from arbitrary URLs; Relix is operator-curated.
Defer the marketplace question until there's a marketplace.

**Decision.** (b) single-tier "operator-explicit opt-in".
Confirmed at Wave 1 close. The policy engine + sensitivity
tags already do fine-grained admission; trust tiers stay
out of the model until a real MCP marketplace exists.

**Status.** answered: defer.

### D-003  Hermes "compression at 50% not 75%" — chronicle event compaction threshold?

**Context.** Hermes compresses LLM context at 50% of the window
(not the base-class default 75%) because 25% headroom lets the
summary + tail fit without immediately triggering another
compression. Relix's chronicle has a different shape (per-task
event log, not per-conversation context window) and the H2
chronicle summarizer milestone (#78) needs a threshold. The
question is at what task-event-count to fire one-line summarization
for terminal-state task chronicles.

**Options.**
- (a) Operator-configurable per-task `max_events_before_compact`,
  default 500.
- (b) Mesh-wide config knob in `[coordinator] chronicle_compact_at`,
  default 500.
- (c) No threshold; one-line summaries are produced on-demand by
  the dashboard / archive job, not eagerly persisted.

**Recommendation.** (c) for first cut — the H2 work ships a pure
summarizer function the dashboard + archive code can call. Adding
a background compactor would (i) require a write-path that mutates
chronicle rows (today's chronicle is append-only) and (ii) make
replay UX less faithful. The summarizer-as-projection is the
honest minimal version.

**Decision.** (c) on-demand only. Confirmed at Wave 1 close.
H2's summarizer stays a pure function; no background
compactor; append-only chronicle invariant preserved.

**Status.** answered: defer.

### D-004  Hermes "skill provenance" — apply to coord-registered tasks?

**Context.** Hermes tags every skill with a `created_by` field
(`agent` vs `user`) using a ContextVar. Relix's coordinator already
stamps `caller: VerifiedIdentity` on every task (M76 propagated this
into chronicle events). The Hermes-grade extension would be: tag
tasks with their *origin context* (chat / dashboard / cli / channel /
flow-engine) so the dashboard can filter `created from chat` vs
`created from dashboard`. Today the `caller` field captures *who*
authorized the task but not *which surface* dispatched it.

**Options.**
- (a) Add `origin_surface` column to `tasks`; populate from the
  bridge's per-route knowledge; expose in the dashboard list view.
- (b) Defer until the dashboard has a filter UX where the column
  would actually show up.
- (c) Reuse the existing chronicle `event.source` field instead of
  a new column.

**Recommendation.** (a) — short additive migration, lights up
existing dashboard list with a useful filter, no replay
implications. ~1 hour of work.

**Decision.** (a) SHIP. Confirmed at Wave 1 close. See
PH-ORIGIN-SURFACE for the implementation milestone.

**Status.** answered: ship.

### D-007  computer_use_tool backend — Relix-owned vs proxy?

**Context.** Hermes ships `computer_use_tool` (mouse/keyboard
/screenshot via VNC or HCB). Real ops value for "the agent
should drive a desktop app." Requires a backend host that runs
a real desktop session the tool can drive. Hermes integrates
with Modal / Vercel Sandbox / Daytona / local X11.

**Options.**
- (a) Ship our own backend (Linux container w/ Xvfb + xdotool).
  Operator self-hosts. Large work but Relix-shaped.
- (b) Proxy through an external service. Small wrapper. Operator
  pays the external service. Less Relix-shaped.
- (c) Defer entirely. Browser automation (CW4) already covers
  most "drive a webapp" cases. Desktop is a different shape.

**Recommendation.** (c) defer. Computer_use is the niche-iest
Hermes tool — most agent workflows are web/CLI. Revisit when
an operator has a concrete desktop-driving workflow blocked
without it.

**Status.** open.

### D-006  Hermes "iteration budget + grace-call" — defer or adapt?

**Context.** Hermes tracks `iteration_budget` per conversation
and allows one `_budget_grace_call` after exhaustion so the model
can write a final summary of what got done. Relix already tracks
`retry_count` + `max_retries` per task (the analogous concept), and
the recovery scan already flips overdue rows. The piece that's
missing is the *grace-call summary*: when a task is about to be
flipped to a terminal failure, one final write that captures the
post-mortem (what was accomplished, what's blocked, what remains).
Hermes does this by giving the LLM one more API call without
tools, asking for a summary. That requires an executor that knows
its budget — Relix's coordinator is a record-keeper today and
does NOT drive execution.

**Options.**
- (a) Add a `task.terminal_summary` capability the bridge / flow
  runner can call before the recovery scan flips a task. Operator-
  visible field on the task row. Doesn't *force* anyone to use it
  but ships the surface.
- (b) Defer until an executor-side context loop exists (Relix today
  doesn't loop — it dispatches one capability and returns). Without
  that consumer the grace-call has no caller.
- (c) Embed the post-mortem into the recovery scan itself: when it
  flips a task to `interrupted`, automatically emit a synthesized
  `task.terminal_summary` event listing last error class, retry
  count, and total wall-clock.

**Recommendation.** (c) — ships the post-mortem with zero new
consumer dependency. The recovery scan already has all the
information it needs (status, retry_count, last_failure_class,
started_at, finished_at). One-line "interrupted after 4 attempts,
last failure: TRANSPORT (DialFailure)" event written before the
status flip. Pure additive change. The Hermes-grade synthesized
summary (option a) lands later when an executor exists.

**Status.** answered:c — shipped via `recover_interrupted` synthesizing
a `task.terminal_summary` event with `auto_emitted_by="recover_interrupted"`,
attempts, retries, wall_clock_secs, last_failure_class, reason. Test:
`recovery_scan_emits_terminal_summary_with_attempt_and_wallclock`.

### D-008  Browser backend — Playwright subprocess vs direct CDP vs headless_chrome crate?

**Context.** CW4 ships `tool.browser.*` as an honest scaffold
(NoneBackend returns `BackendNotConnected` on every non-noop
call). Wave 1 demands a real backend. Three concrete paths:

**Options.**
- (a) **Playwright via Node.js subprocess** — spawn `playwright-core`
  in a sidecar process, JSON-RPC bridge over stdio.
  Pros: maximum compatibility (Hermes uses this exact pattern;
  every site that works in Hermes will work in Relix);
  Playwright's auto-wait + selector engine is best-in-class.
  Cons: a Node runtime becomes a hard runtime dependency of
  the tool node; cross-process IPC adds latency and a
  failure surface; operator must `npm i playwright-core` once.
- (b) **Direct Chrome DevTools Protocol via `headless_chrome`
  crate** — pure-Rust CDP client; spawns a headless Chromium
  the crate downloads itself. Pros: single binary, no Node
  dep, fast. Cons: less battle-tested than Playwright; some
  modern auto-wait patterns must be hand-rolled; crate
  maintainership is moderate (active but not heavy).
- (c) **`fantoccini` (WebDriver) + Selenium / geckodriver** —
  W3C-standard protocol. Pros: portable across browsers
  (Firefox + Chrome). Cons: WebDriver is slower + chattier
  than CDP; selector ergonomics worse; another long-running
  daemon to manage.

**Recommendation.** (b) `headless_chrome` for first cut.
Rust-native, single binary, no Node runtime dependency, fast
enough for the operator workflows that matter today (navigate
+ extract + screenshot + click). Wave-2 can add a Playwright
subprocess backend behind a feature flag for sites that need
its richer auto-wait. Honest fallback: when no Chromium can
be found / launched, the backend continues to return
`BackendNotConnected` with the specific reason — never fakes
success.

**Status. SHIPPED.** PH-BROWSER-D008-RESOLVE: all three live
drivers landed alongside PH-BROWSER-FEATURES (multi-backend
feature plan).

Operator picks at runtime via `[tool.browser] backend = "..."`
from `{none | headless_chrome | playwright | webdriver}`;
each non-`none` backend is gated on its own Cargo feature
(`browser-headless-chrome` / `-playwright` / `-webdriver`,
plus the convenience `browser-all`).

Selecting a backend whose feature isn't compiled fails
LOUDLY at startup (`ToolError::Build`) — no silent
`NoneBackend` fallback. The three live drivers:

- **PH-BROWSER-HC** (`headless_chrome` crate) — Chrome
  DevTools Protocol against the operator's existing
  `chrome` / `chromium` binary. Recommended default per the
  original D-008 analysis.
- **PH-BROWSER-PW** (Node + `playwright-core` sidecar over
  stdio JSON-RPC) — best multi-engine coverage; heaviest
  install.
- **PH-BROWSER-WD** (`fantoccini` crate against
  operator-supplied `chromedriver` / `geckodriver`) — most
  W3C-standards-aligned; requires a separate driver
  binary.

Each backend is lazy on the browser/driver launch so the
tool node starts cleanly even when the runtime isn't
present — the first call returns
`BackendNotConnected { reason: "<backend>: <specific
cause>" }`. Honest contract preserved: no fake success, no
silent NoneBackend downgrade.

Per-runtime integration tests are gated on the runtime
being present (Chrome in PATH for HC, `node` +
`playwright-core` for PW, a live driver responding at
`/status` for WD); CI without those skips with `eprintln!`
and the unit tests still pass.

Reference: `docs/browser-tool.md` for the build matrix
(`--features browser-headless-chrome` etc.) and the
"Recommended default" section.

**Context.** Hermes uses `contextvars.ContextVar` to thread the
write-origin label through nested async tool calls without
contaminating sibling tasks. Tokio's analogue is `tokio::task_local!`.
This would let Relix capture e.g. "this `task.update` came from the
recovery scan, not from the AI node" without threading a parameter
through every dispatch.

**Options.**
- (a) Adopt `task_local!` for the equivalent of write-origin
  threading. Adds a small amount of plumbing in dispatch.
- (b) Pass an explicit `origin` parameter on `InvocationCtx` —
  no magic, easier to grep.
- (c) Skip — `caller: VerifiedIdentity` is already enough for the
  audit log; this is over-engineering.

**Recommendation.** (b) for the cases where the distinction matters
(recovery scan, scheduler, AI-node dispatch). Easier to reason
about and matches Relix's existing prefer-explicit-args posture.

**Status.** open.

### D-009  MCP stdio runtime — which real server to bind against?

**Context.** CW5 ships the MCP registry + discovery scaffold;
`tool.mcp.invoke` returns `RuntimeNotConnected` always.
PH-MCP-PROTO ships the JSON-RPC wire layer as a tested data
module (no I/O). The next step (PH-MCP-STDIO1) wires it into
the registry: spawn the operator-declared stdio process,
initialize handshake, tools/list cache, tools/call dispatch.

That step is blocked by something subtle: there is no concrete
MCP server target identified in this codebase. Continuation
state explicitly warned: "Avoid CW5 mcp_tool until a real MCP
server target is identified." Without a target, the protocol
implementation has no end-to-end test path and risks
implementing against an imagined server shape that the operator
will eventually replace.

**Options.**
- (a) Pick the official MCP SDK reference server
  (https://github.com/modelcontextprotocol/servers) and bind
  against its `filesystem` example. Stable, official-shaped.
- (b) Bind against Anthropic's MCP server bundle. Requires the
  operator to install the bundle independently. Tighter
  ecosystem alignment.
- (c) Defer all MCP stdio runtime work until an operator
  brings a concrete server target. Ship only the protocol
  layer (PH-MCP-PROTO) which is pure data + safe.

**Recommendation.** (c) — defer the runtime wiring. The
protocol layer is honest and forward-compatible; binding to a
specific server without operator-confirmed need risks the same
"shipped scaffold, no real consumer" problem CW5 was meant to
avoid. The protocol layer alone is testable and gives the next
session a head start the moment a target is named.

**Status.** **shipped** as of PH-MCP-RUNTIME. The runtime now
spawns operator-declared `stdio` MCP servers lazily on first
`tool.mcp.invoke`, runs the `initialize` handshake, and
dispatches `tools/call` + `tools/list` against the live process.
The operator picks the server in their TOML (`command`,
`args`); Relix doesn't bind to any specific reference server.
Integration test gates against `@modelcontextprotocol/server-everything`
(skipped when node/npx are absent — CI without node still
passes). HTTP transport remains `RuntimeNotConnected`.

### D-010  Full PTY backend for `tool.terminal.shell.*` — adopt `portable-pty`?

**Context.** PH-TERM-SHELL spawns shells with stdin attached to a
regular OS pipe, NOT a pseudo-terminal. Programs that check
`isatty()` switch to non-interactive mode; programs that rely on
TTY-driven signal delivery (e.g., bash translating `0x03` on stdin
into SIGINT for the foreground job) do not see the signals.
PH-TERM-CONTROL ships `tool.terminal.shell.control` for named
control bytes, but it can't fix the no-PTY behavior — the bytes
arrive on the input pipe as ordinary stdin.

The full fix is PTY allocation via `portable-pty` (wezterm's
cross-platform crate covering Unix `openpty`/`forkpty` and
Windows ConPTY).

**Options.**
- (a) Adopt `portable-pty` as a workspace dep + add a new
  `SpawnMode::Pty` variant. The architectural mismatch is real:
  `portable-pty`'s `Child` does NOT implement tokio's async
  APIs, so the existing `validate_and_spawn` /
  `drive_to_completion` cannot be reused. New code paths
  needed for spawn, drain, wait, cancel — likely a separate
  PTY module of ~400-600 LOC plus a sync-to-async bridge via
  `tokio::task::spawn_blocking` and channels.
- (b) Defer — accept that `tool.terminal.shell.*` is
  "non-interactive shell on stdin" and document the gap
  honestly. Operators driving TUI / isatty-checking programs
  continue without Relix support.
- (c) Ship a narrower Unix-only PTY backend via `nix` (avoids
  the cross-platform sync I/O complexity of `portable-pty`).
  Smaller scope but operators on Windows lose PTY entirely.

**Decision.** (a) — SHIPPED behind `--features terminal-pty`
(PH-TERM-PTY). Operators flip `[tool.terminal] pty = true` AND
build with the `terminal-pty` Cargo feature to get a real
pseudoterminal via the `portable-pty` crate. The default
pipe-based path is unchanged — operators pick. Selecting
`pty = true` without the feature is a loud startup error at
`ToolBackend::new` time (no silent fallback, matching the
PH-BROWSER-FEATURES posture). TUI / isatty workflows
(vim, top, interactive REPLs through `tool.terminal.shell.*`)
now work; output may include ANSI escape sequences, which the
chronicle audit captures verbatim — ANSI stripping is left to
the consumer.

**Status.** SHIPPED behind `--features terminal-pty`
(PH-TERM-PTY). The pipe path remains the default; opt-in PTY is
purely additive.

---

## Answered

- D-006 — answered:c (in-place above). Recovery scan auto-emits a
  synthesized `task.terminal_summary` event. Pure additive change.

---

## Superseded

(none yet)
