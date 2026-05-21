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

**Status.** open.

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

**Status.** open.

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

**Status.** open.

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

**Status.** open.

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

### D-005  Hermes "ContextVar for write-origin scoping" — adopt for telemetry?

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

---

## Answered

- D-006 — answered:c (in-place above). Recovery scan auto-emits a
  synthesized `task.terminal_summary` event. Pure additive change.

---

## Superseded

(none yet)
