# Relix — Living Product Roadmap (current)

> **Status:** Canonical working roadmap. This is the *map from the design docs to the
> code that exists today*, plus the next implementation queue. Read this **and** the
> cited design-doc section **before** starting any product work.
>
> **Source of truth, in order:** the Paperclip audit under `references/paperclip/` captures
> the product instincts Relix is trying to learn from; the design docs in `docs/` (see
> CLAUDE.md) define Relix's intended adaptation; `docs/product-spine-implementation.md` is
> the **audited implementation map + divergence ledger** (what the code actually does
> today); this file is the **concise roadmap that ties them together and orders what's
> next**. When they disagree, the Paperclip audit wins on "what Paperclip actually
> felt/built like," the Relix design docs win on Relix-specific intent, and the ledger wins
> on "what is true right now" — fix the gap, don't paper over it.
>
> **Last reconciled:** 2026-06-06 against the implementation ledger (through commit
> `1f648871`, Allowance calendar-month windowing + Guild autonomous hard-stop era), the
> Paperclip audit files listed below, and the design docs listed below.

Paperclip audit sources this roadmap must stay grounded in (read these before product
direction work, not as optional inspiration):
`references/paperclip/RELIX_PAPERCLIP_AUDIT_LOG.md` ·
`references/paperclip/.relix-audit/paperclip-file-line-coverage-summary.md` ·
`references/paperclip/.relix-audit/paperclip-file-line-coverage-progress.md` ·
`docs/hermes-vs-paperclip-vs-relix.md`.

Design docs this roadmap is built from (read these, not vibes):
`relix-lexicon.md` · `relix-company-model.md` · `relix-execution-and-issue-design.md` ·
`relix-dashboard-design.md` · `relix-hermes-integration.md` · `relix-agent-adapters.md` ·
`product-spine-roadmap.md` · `product-spine-implementation.md` · `current-limitations.md` ·
`live-smoke.md`.

---

## 1. Product North Star

**Relix is a company of AI employees you govern — a crew operating console, not a task dashboard.**

You hand Relix a goal in plain language. A **Prime** (the apex Operative) proposes a
strategy and a team, you **greenlight** it, and a **Guild** of **Operatives** works
**Briefs** in **Shifts** at their **Bench**, escalating up the **Line**, spending against
an **Allowance**, every boundary-crossing action passing a **Clearance** and landing in the
immutable **Chronicle**. You watch the whole operation at a glance from **The Desk**:
*who's doing what, what it costs, and whether it's working* — while the heavy machinery
(signed mesh, policy admission, hash-chained audit, sandboxed execution) stays hidden until
you need it.

The Paperclip audit is binding on the product feel: Paperclip is not only a polished
dashboard. It is a company control plane where issue execution, heartbeat/run orchestration,
agent runtime, workspace runtime, plugin hosting, access/resource membership, secrets,
recovery, company portability, issue detail/chat/run transcript, agent management,
company/project/workspace surfaces, routines, search, dashboards, and tested UI states
connect through shared contracts.

The Paperclip-inspired shift (`relix-company-model.md` §1, §8): the product is organized
around **work objects and the org** (Guild → Mandate → Campaign → Brief → Shift, and the
Operative org tree), **not** around a panel-per-capability control plane. The 22 legacy
feature panels demote to detail tabs; the top-level surfaces are the company and its work.
What makes Relix *not* Paperclip stays underneath: a decentralized signed mesh, per-Operative
**Keys**, an enforced **strategy gate**, and a universal **Rig** adapter system that can run
any agent backend.

The lexicon (`relix-lexicon.md`) is binding on every product-facing surface. Internal
identifiers (`tasks`, `agent`, `reports_to`) stay stable; net-new code adopts the lexicon
directly.

---

## 2. Current Completed Capabilities

Grounded in `product-spine-implementation.md` (the "Shipped This Roadmap" map) and the git
history, where **every commit cites the design-doc section it implements** — the discipline
the founder asked to be able to verify. Examples: `b5097fc3`/`8d6a083b`
(company-model §12.5B/§12.6), `74d96538` (execution-and-issue §1.9), `c34f13d7`
(dashboard-design §11), `579fa8c5` (Action Center live spend).

### Company / Crew (`relix-company-model.md` §4, §12.6)
- **Founder bootstrap + Starter Crew** — `company.bootstrap_founder` (idempotent Founder),
  `company.starter_crew` (safe-local `echo` Operatives, no external auth) closes the
  empty-company → working-crew loop as the Founder's sovereign first-run action.
- **Crew status & org shape** — `company.status` returns Prime + crew counts, by-status,
  by-role, reports-to tree.
- **Governed hiring** — pending hires are inert until greenlit; `agent.approve_hire`
  binds a Rig atomically at approval so a greenlit Operative is immediately runnable.
  `agent.create` is operator-only; agent-originated hires require the **spawn Key**;
  `spawn_route=lead/founder` mints a real typed **Clearance**.

### Mandates / Strategy gate (`relix-company-model.md` §5.5, §12.5)
- **Enforced strategy gate** — `mandate.strategy.{status,propose,approve,reject}`;
  materialization refused until strategy is approved.
- **Persistent team plans + live readiness** — `mandate.team_plan` (durable),
  `mandate.team_readiness` recomputes from real hire/Clearance state (no faked readiness);
  reuses active same-role crew before filing hires.
- **Orchestration** — `mandate.orchestrate` (`plan_only` / `create_briefs` /
  `assign_ready`) builds a deterministic, idempotent 3-tier Brief tree; missing/pending/
  blocked roles get durable placeholder tracks.

### Briefs / Workroom (`relix-execution-and-issue-design.md` §1, §1.9)
- **Two-pointer Claim** — `checkout_run` + `execution_run`, self-refresh, lease/release,
  lock clearing on assignee/state change (the LOCKED model, §7.1). A Claim **conflict** on
  the run start path now returns **HTTP `409`** (never a retryable `200`), an in-process
  **per-Operative start lock** serializes concurrent starts, and a **same-Operative
  duplicate-start guard** refuses a *new* start (`already_running` → `409`) when that
  Operative already has a live, actually-running run on the same Brief — so a double-start
  can no longer open two run rows/workspaces while the lower-level `claim_brief_for_run`
  stays idempotent for wakeup/heartbeat/recovery (§1.4/§2.6). **Stale-run adoption by
  terminal evidence** now also ships (§5 slice 10): a dangling **live** Claim whose run
  pointer (`execution_run_id`/`checkout_run_id`) points at an already-**terminal**
  `brief_runs` row is reclaimed at start time (`reclaim_terminal_claim`, called in
  `preflight_run` before the claim), so a new start proceeds on terminal evidence instead
  of waiting for the age-based `recover_stale_runs` sweep — safe by construction (never
  releases a Claim backing a still-`running` run, a Claim with no matching run evidence, or
  a newer Claim) and chronicled `brief.claim_reclaimed`. *Remaining edge:* Relix
  releases+re-claims rather than transferring the dead owner's checkout context in place
  (full Paperclip "adopt the prior checkout run").
- **Entry guards** — `in_progress` requires assignee + no unresolved Snags; `in_review`
  requires a real reviewer.
- **Brief detail API** — `brief.detail` returns the full product object (fields,
  sub-briefs, parents, snags, dossiers, labels, due, claim, `latest_run`, chronicle).
- **Thread interactions** — answerable `ask` / `confirm` / `suggest_tasks` cards
  (`brief_interactions` table), with governed assignee hints, backward-only `after`
  dependencies, idempotent accept, children inheriting parent context. **Approval-bound
  plan confirms** (`brief.plan_confirm_open`, §1.8) bind a `confirm` to the latest `plan`
  Dossier revision; a stale accept (newer plan revision, or superseded by a comment)
  expires the card and never resolves as approved. Now usable from the dashboard: a
  `POST /v1/spine/briefs/:id/plan-confirm` bridge route + a workroom **Request approval**
  control open the bound confirm, the `expired` status renders distinctly from `rejected`,
  and a "bound to plan" cue shows on the card. **Plan packages** (`brief.plan_package_open`,
  §1.7/§1.8/§3.1) go one step further — a confirm linked to **both** a `plan` Dossier and a
  `suggest_tasks` proposal (`bound_interaction_id`); accepting it via `brief.plan_confirm_respond`
  materializes the linked proposal exactly once through the resumable decomposition ledger.
- **Desk / Inbox reads** — `/v1/spine/inbox`, `/v1/spine/briefs/:id/thread`,
  `/v1/spine/unassigned`; board cards surface unresolved same-Guild blockers.
- **Supervisory auto-wake** (`execution §1.6/§3.1`) — the central `set_board_status`
  transition seam promotes first-class follow-up wakes, event-driven (no busy-poll): a Brief
  reaching `done` wakes every same-Guild dependent whose blockers are now all resolved
  (`blockers-resolved`); a child reaching `done`/`cancelled` wakes a same-Guild parent once
  all its same-Guild Sub-briefs are terminal (`children-completed`). Both go through the
  persistent wakeup queue's shared enqueue (coalesce/defer/skip — no duplicate runs),
  tenant-safe, and honest when a target has no assignee (a `brief.wakeup_skipped` Chronicle
  note, never an invented assignee). A `cancelled` blocker never resolves a dependent (LOCKED).

### Runs / Shifts / Rigs (`relix-agent-adapters.md`; execution §run-artifacts)
- **Universal Rig probe** — `ProcessRig::probe()` returns six honest statuses; dashboard
  refuses to assign an unavailable adapter.
- **Live CLI adapters** — Claude (`claude --print --output-format stream-json`) and Codex
  (`codex exec --json`) parsed for outcome/usage; both live-validated end-to-end on Windows.
- **Async dispatch + unified chokepoint** — every path (manual `brief.run`, Prime-started
  Shift, heartbeat) funnels through `prepare_claimed_run` → `execute_ready`.
- **Durable run ledger + transcript + cancel** — `brief_runs`, `run_events` (capped),
  `CancelRegistry`; durable **refused**-run rows with machine reasons.
- **Reviewable result → safe apply** — before/after workspace scan → `run_artifacts`
  (metadata + redacted preview, no content), per-file diff, `run.apply` with baseline-hash
  conflict detection; **clean apply is review-to-done** and unblocks dependents.
- **Scoped per-Brief workspace** — `<root>/<run_id>`, context modes `empty` / `copy_repo`,
  hard caps, secret/`.git`/generated-dir exclusions.
- **Reliability pack** — usage/cost capture, `agent_runtime_state` (resumable session id),
  boot recovery (`recover_stale_runs` → `interrupted`, releases Claim), SSE event stream
  (`/v1/runs/events/stream`).

### Governance / Safety (`relix-company-model.md` §5.2, §6; lexicon §Governance)
- **Keys enforced** — spawn, assign (every assignment path), manage, configure, secret
  allowlist (deny-by-default), instruction-bundle-as-charter.
- **Allowance hard-stop** — heartbeat refuses a Brief when the Operative is over Allowance
  (**current UTC calendar-month** window via the canonical `heartbeat::allowance_window`,
  best-effort); `brief.budget_refused` event.
- **Guild spend hard-stop (autonomous)** — the autonomous heartbeat path now also
  refuses a Brief when its **Guild** is over its monthly budget, mirroring the
  per-Operative stop and additive on top of it (`guild.budget_refused` event,
  `over_guild_budget` refused run). Tenant-safe: the Guild spend is summed over
  only the Brief's own Guild's active Operatives (`company-model §6/§6.6`). Manual
  `brief.run` / `prime.start` stay sovereign (no Guild gate).
- **Tenant isolation** — product agent/governance routes are tenant-scoped; a known id from
  another Guild resolves not-found.
- **Chronicle** — hash-chained events for every run transition, interaction, and Prime
  action; **durable activity ledger** `/v1/activity/recent` (`bridge-activity.jsonl`).
- **Live spend telemetry** — Action Center surfaces real month-to-date spend vs Allowance
  through the gate's own `MetricsSpendSource` (commit `579fa8c5`, ledger-reconciled in
  `177c93ef`); the window is the canonical UTC calendar month (`allowance_window`), the
  exact source + window the dispatch gate enforces.
- **Canonical Guild spend route** — `guild.spend` (`GET /v1/spine/guild/spend`) exposes the
  Guild's current-UTC-month spend as a numeric object (`spent_micros`/`spent_cents`,
  `budget_cents`/`remaining_cents`/`over_budget`, `window_start_ms`/`resets_at_ms`/`now_ms`,
  `source`/`computed_from`). It is the SAME ledger figure + window the autonomous Guild
  hard-stop enforces, via the single shared `heartbeat::guild_spend_micros` helper (the gate
  was refactored to call it) — so the dashboard Costs card and the gate can never disagree.
  Tenant-safe (sums only the caller's own Guild's active Operatives); no metrics ledger →
  honest null spend (`company-model §6/§6.6`).
- **Allowance calendar-month windowing** — the per-Operative Allowance and Guild-budget
  hard-stops bill against the **current UTC calendar month** via a single canonical
  `heartbeat::allowance_window(now_ms)` (inclusive month start → reset edge), replacing the
  trailing-30-day approximation. Reset is implicit (spend is re-summed from the live month
  start); the gate and the Action Center read the identical window so they can never
  disagree (`company-model §6/§6.6`).

### Dashboard (`relix-dashboard-design.md`)
- **React SPA is THE dashboard** — `apps/dashboard` built to
  `crates/relix-web-bridge/dashboard-dist`; legacy `dashboard.html` and `spine_dashboard.html`
  **deleted**; `/spine` is a 308 → `/dashboard`; missing bundle returns honest 503; a
  dist-parity gate (CI + `scripts/check-dashboard-dist.ps1`) keeps the committed bundle in sync.
- **Work-object IA** — Overview, Briefs (board + drag-drop + contextual detail panel),
  Agents (Roster + per-Operative Keys panel), Mandates (governed strategy→orchestrate
  workflow), Runs, Chat (Prime), Company, Settings, Scheduled.
- **Action Center / The Desk** — `GET /v1/spine/company/actions`: ranked next-actions with
  severity chips, plain-language reasons, recovery-decision cards (root cause → one route),
  refreshed off SSE + low-frequency poll.
- **Brief workroom** — Conversation thread + Chronicle ledger + answerable Requests panel;
  Shift lifecycle operated inline.
- **Shell** — mobile off-canvas drawer + ⌘K command palette (navigation-only); client-side
  **invalidation bus** (`c34f13d7`) for surgical refresh after mutations.
- **Ops** — `relix dashboard doctor` (read-only health/auth probe), `reset-admin`
  (Argon2id recovery), Maintenance & Storage panel (bounded scan + gated prune), local
  backup scripts.

### Verified end-to-end
- **Live smoke** (`docs/live-smoke.md`) drives a fresh user: boot → login → starter crew →
  `prime.propose` → `approve` → `start` → echo Shift → review → apply → Brief `done`, over
  real HTTP, with a boot-policy coverage guard (`scripts/check-boot-policy-coverage.ps1`,
  CI job `boot-policy coverage`) so no live route ships unadmitted.

---

## 3. Remaining Major Gaps (prioritized, grounded in the ledger)

Tagged **[BE]** backend, **[FE]** frontend, **[DOC]** docs-only. Each cites the divergence
ledger entry or design section.

**P1 — correctness & governance honesty**
1. **[BE] Claim two-pointer model — DONE.** The `409` conflict surface, the per-Operative
   start lock, and the **same-Operative duplicate-start guard** shipped in slice 1
   (`brief.run` maps a Claim conflict `already_running` → HTTP `409`, never a retryable
   `200`; an in-process per-Operative start lock serializes concurrent starts; a new start
   by the same Operative on a Brief it is already running is refused `already_running`
   instead of opening a second run row/workspace; "never retry a 409" pinned in tests). The
   last open piece, **stale-run *adoption by terminal evidence***, now also shipped (§5
   slice 10): a dangling **live** Claim whose run pointer references an already-**terminal**
   `brief_runs` row is reclaimed at start time (`reclaim_terminal_claim` in `preflight_run`),
   safe by construction and chronicled `brief.claim_reclaimed`, so a new start proceeds on
   terminal evidence without waiting for the age-based `recover_stale_runs` sweep
   (`execution §1.4`/`§7.1` LOCKED; ledger "Claim HTTP 409 + per-Operative start lock +
   duplicate-start guard" = DONE, "stale-run adoption" = DONE). *Remaining edge (deferred):*
   Relix releases+re-claims rather than transferring the dead owner's checkout context in
   place (full Paperclip "adopt the prior checkout run"); the reclaim is wired into the
   manual/Prime start chokepoint, while the autonomous heartbeat path still relies on the
   age-based sweep (a live-claimed Brief is not `ready`, so the heartbeat never races it).
2. **[BE] Guild-level spend hard-stop** — **SHIPPED for autonomous dispatch** (roadmap §5
   slice 2): the heartbeat path now refuses a Brief when its Guild is over its monthly
   budget, mirroring the per-Operative hard-stop and additive on top of it
   (`guild.budget_refused` / `over_guild_budget`), tenant-safe (the Guild spend is summed
   over only the Brief's own Guild). Manual `brief.run` / `prime.start` stay sovereign
   (operator-initiated, no Guild gate). *(The issue-tree cost rollup + billing-code
   attribution backend is now SHIPPED — see §P1 slice 3b; the spend window is the UTC
   calendar month with reset — slice 9 = DONE. The delegation-depth counter + guard backend
   is now SHIPPED — see §P1 slice 3c. Object-level (Mandate/Campaign/Guild) billing codes
   are now SHIPPED too — see §P1 slice 3b. Remaining deferred: the frontend Costs surface.)*
3. **[BE] Allowance windowing** — **DONE** (§5 slice 9). The per-Operative and Guild
   hard-stops + the Action Center live-spend feed now bill against the **current UTC
   calendar month** via the single canonical `heartbeat::allowance_window(now_ms)`
   (inclusive month start → reset edge), replacing the trailing-30-day approximation; reset
   is implicit (spend re-summed from the live month start).
3b. **[BE] Issue-tree cost rollup + billing-code attribution** — **BACKEND SHIPPED**
   (`company-model §6.6`). `brief.cost_rollup` (→ `GET /v1/spine/briefs/:id/cost`) sums the
   durable `brief_runs` ledger over a Brief **and its same-Guild Sub-brief tree** (own vs
   descendant totals, tree counts, per-billing-code breakdown), tenant-safe by construction
   and windowed on the canonical `allowance_window` (overridable since/until). Billing code is
   an additive `tasks.billing_code` (set via `brief.set`, on `BriefFields`) + a
   `brief_runs.billing_code` **stamped at run start** for manual + autonomous runs alike.
   **Object-level billing codes are now BACKEND SHIPPED:** additive `billing_code` on
   Mandate, Campaign, and Guild (set via `mandate.update`/`campaign.update <id>|billing_code|<code>`
   and the new `guild.set_billing_code`; surfaced on the Mandate/Campaign/Guild reads), with the
   run-stamp inheritance now resolving **Brief own → nearest same-Guild ancestor Brief →
   linked Campaign → linked Mandate → Guild**. The object fallback is injected into the Brief
   ledger as a tenant-safe `ObjectBillingResolver` (the spine store): a Brief in one Guild can
   never inherit another Guild's Campaign/Mandate/Guild code even with a bad/cross-Guild link,
   and a later object-code change never rewrites a past run's stamp (point-in-time). *Still
   deferred:* the **frontend** Costs surface (§P2 slice 5).
3c. **[BE] Delegation-depth counter + guard** — **BACKEND SHIPPED** (`company-model §6.6`).
   The runaway-recursion safety backstop that complements 3b. A Brief's **delegation depth** =
   the longest same-Guild `spawned` parent chain up to a root (root `0`, Sub-brief `1`, …),
   via `brief_delegation_depth`. The central cap `MAX_SUBBRIEF_DELEGATION_DEPTH = 1024` is the
   doc-LOCKED "≥1024 runaway backstop, not a product limit" (`execution` Part 7 item 2).
   `link_subbrief` — the single choke point for direct `brief.subbrief`, the `suggest_tasks`
   accept materialization, AND Mandate orchestration — refuses a link whose child would exceed
   the cap (no edge created); the `suggest_tasks` accept pre-checks up front so an over-cap
   accept refuses with **no partial child creation** and the card stays open. Tenant-safe:
   depth is computed over same-Guild edges only, so a cross-Guild edge can't inflate/leak
   another Guild's depth. `brief.detail` now surfaces `delegation_depth` + `max_delegation_depth`.
   *Honest gap:* orchestration links via `let _ = link_subbrief(...)`, but its tree is only 2
   deep, so the cap never fires there. *Still deferred:* the frontend Costs/Lattice surfaces
   that would render depth.

**P2 — product-feel surfaces (mostly frontend on data that already exists)**
4. **[FE] The Lattice (org chart)** — **FRONTEND SHIPPED (partial)** (`dashboard-design §9`).
   `apps/dashboard/src/pages/Lattice.tsx` (nav `/lattice`) renders the live `reports_to`
   forest from `/v1/spine/operatives` (+ `/v1/spine/company` for apex order) as an SVG-edge +
   node-card tree, role/status/rig chips + direct-report counts, a live pill driven by
   `/v1/runs`, and click → a per-Operative detail (Keys + allowance + risk ceiling via
   `/v1/spine/keys/:id` + `/v1/agents/:id`). *Partial (honest):* full drag-pan/pinch is
   **deferred** — the surface ships a scrollable stage (overflow:auto = pan) with explicit
   −/reset/+ zoom controls instead, to stay CSP-clean and dependency-free.
5. **[FE] Full Costs surface** — **SHIPPED** (`dashboard-design §10`).
   `apps/dashboard/src/pages/Costs.tsx` (nav `/costs`): the Guild budget card now reads
   **canonical month-to-date Guild spend** from the dedicated `guild.spend` route
   (`GET /v1/spine/guild/spend`) — the EXACT ledger figure + UTC-calendar-month window the
   autonomous Guild hard-stop enforces (via the shared `heartbeat::guild_spend_micros` over
   `heartbeat::allowance_window`), so the card can never disagree with the gate. The card shows
   budget vs **actual spent** vs remaining (over-cap = red bar + "over budget" chip), the reset
   date, and the committed Allowance kept as a clearly-DISTINCT *capacity-reserved* figure. Also:
   per-Operative allowance (Keys) + observed spend (`/v1/metrics/agents`, windowed), the
   Brief-tree rollup (`brief.cost_rollup` → `GET /v1/spine/briefs/:id/cost`) with own/descendant
   split + per-billing-code breakdown, and budget/over-cap incidents (the `budget`-category
   Action Center items). *Honest remaining nuance:* the per-agent "observed spend" table is
   still **operational telemetry** from the observability **metrics window** (24h/7d/30d),
   explicitly labelled distinct from the governance calendar-month, and its metrics↔Operative
   join stays best-effort by agent name/id — but the **Guild** month-to-date figure is now
   canonical, not an approximation.
6. **[FE] Run transcript renderer** — **FRONTEND SHIPPED** (`dashboard-design §8`).
   `apps/dashboard/src/components/RunTranscript.tsx`: block-grouped "nice"/"raw" view over the
   real `/v1/runs/:id/events` stream (lifecycle rail, assistant/result cards, collapsible tool
   groups, denied/error callouts, usage/cost chip), live-tailed via the run-event SSE with a
   polling fallback + honest connection chip. Used on the Runs page and embedded in the Brief
   workroom.
7. **[FE] Streaming Brief thread** — **FRONTEND SHIPPED (partial)** (`dashboard-design §7/§8`).
   The Brief workroom embeds the live run transcript inline (`<RunTranscript>` Live-work block);
   interaction cards refresh on the run-event SSE via the detail's existing `reload()`.
   *Honest gap:* no **dedicated** interaction-card SSE — a card raised without a run transition
   surfaces on the next run event / manual refresh, not instantly (the design's "one socket
   streams cards" remains future).
8. **[FE] Approvals + Settings hubs** — **FRONTEND SHIPPED (partial)** (`dashboard-design §10`).
   New `/approvals` page + nav/palette entry: pending **Clearances** from `/v1/spine/clearances`
   (unified `coord.approval.pending` queue — spawn-hire/strategy/budget/high-risk, decided inline
   via `/v1/spine/clearances/:id/decide`), plus direct **pending hires** + **budget alerts** from
   `/v1/spine/company/actions` (hire approve/reject via `/v1/agents/:id/approve-hire|reject-hire`).
   A pending-Clearance nav badge; decisions invalidate the actions/mandates/briefs surfaces.
   Settings hub gains an **Admin · session recovery** section over `/v1/runs/runtime-state`
   (per-agent lookup + gated reset) on top of the existing Health/Maintenance/Adapter/run-sandbox/
   heartbeat sections. *Honest gaps:* the budget-alert decision still lives on its own route (no
   inline decide route exists); strategy/budget/high-risk Clearances decide through the same generic
   `decide` (no per-type typed payload editor yet); runtime-state is per-agent (the route requires an
   `agent_id`), so there is no global session list.

**P3 — depth / autonomy**
9. **[BE/FE] Smarter companion** — `prime.propose` AI mode is opt-in and rule-validated;
   replacing the deterministic planner with an LLM driving the governed spine APIs is
   future (`current-limitations.md`; ledger "Mandate orchestration" still not autonomous).
10. **[BE] Exactly-once decomposition + auto-wake promotion** — **both parts are now BACKEND
    SHIPPED** (exactly-once decomposition partial; see below). **Auto-wake promotion**
    (`execution §1.6/§3.1`; see §5 slice 12). When a Brief reaches a
    terminal column at the central `set_board_status` seam, Relix sequences follow-up work
    event-driven (no busy-poll): a `done` Brief promotes a `blockers-resolved` wakeup to each
    same-Guild dependent that is now fully unblocked, and a `done`/`cancelled` child promotes a
    `children-completed` wakeup to a same-Guild parent once all its same-Guild Sub-briefs are
    terminal — through the existing persistent wakeup queue (coalesce/defer/skip, no duplicate
    runs), tenant-safe, and honest about a missing assignee. The **cost-tree rollup +
    billing-code attribution** part of this line is also **backend SHIPPED** (see §P1 slice 3b);
    only the frontend Costs surface (§P2 slice 5) consumes it. **Exactly-once plan
    decomposition** (`execution §1.7`) is now **BACKEND SHIPPED (partial)** too: the
    `suggest_tasks` accept path is backed by a durable **decomposition claim/ledger**
    (`brief_decomposition_claims`, keyed by `(task_id, interaction_id)`) so accepting a child-Brief
    plan is **resumable and never double-creates children**. The claim row — not the card flip — is
    the linearization point and carries a **proposal fingerprint** (BLAKE3 over the normalized
    plan's materialization-affecting fields, so cosmetic/summary changes don't matter), a
    **`created_ids` resume cursor** (each child id persisted via compare-and-swap *before* the next
    child is created), `plan_len`, `owner`, and `status` (`in_progress`→`complete`). Net effect: a
    duplicate accept **no-ops** (returns the same ordered ids), a crashed accept **resumes from the
    cursor** (creating only the missing children, then idempotently re-links + wires `after`→Snag
    edges), and a re-accept whose proposal hashes differently is **refused** (an accepted plan
    cannot fork). **Concurrent double-accept is orphan-free:** a per-decomposition in-process
    materialization lock (one `Mutex<()>` per `(task_id, interaction_id)`, mirroring the per-Operative
    start lock) serializes the whole accept so two racing accepts/resumes can never interleave a
    child create with its cursor record — the loser blocks then no-ops or resumes, never leaving an
    unlinked orphan child Brief (proven by a two-thread barrier race test). **Owner takeover is now
    enforced (`execution §1.7`):** because the accept is **operator-driven and synchronous**, the
    claim's `owner` is the **accepter** — not a live run with a heartbeat — so there is no real
    liveness pointer to probe. The resume path enforces a **conservative owner guard with stale-age
    takeover** (`DECOMPOSITION_OWNER_STALE_SECS`, 15 min): the **same** owner may always resume; a
    **different** responder is **refused** on a still-**fresh** `in_progress` claim and may **take
    over** only a **stale** one (untouched past the threshold ⇒ the owning process crashed) or a
    **terminal** one (a `complete` claim no-ops for anyone). A takeover reassigns `owner` and
    Chronicles `brief.suggestion_taken_over`; the **fingerprint check runs first**, so a forked plan
    still refuses even when the claim is stale. Correctness never depends on the guard — the lock +
    cursor + fingerprint already guarantee exactly-once; the guard only stops a *second* operator
    from racing in on a decomposition another operator is actively driving. All prior governance
    (parent context inheritance, assign-Key-gated hints, tenant
    isolation, delegation-depth) is unchanged. **Approval-bound plan *confirm* is now BACKEND
    SHIPPED (first slice, `execution §1.8`):** a new `brief.plan_confirm_open` capability opens a
    `confirm` **bound to the Brief's latest `plan` Dossier revision** (the bound Dossier id IS the
    revision — Dossiers are immutable, append-only rows; recorded on the card as
    `bound_doc_id`/`bound_doc_kind` and chronicled). It **refuses when no `plan` Dossier exists**;
    on **accept** it re-checks the latest `plan` revision is still the bound one — if a newer `plan`
    Dossier was attached (or the operator **superseded it by commenting**), the accept is **refused
    as stale**, the card flips to `expired`, and it **never resolves as approved** against a
    superseded plan. Plain confirms are unaffected; duplicate answers stay typed/idempotent;
    tenant-isolated (cross-Guild reads as not-found). **Dashboard control now shipped:** a
    `POST /v1/spine/briefs/:id/plan-confirm` bridge route proxies the capability and the Brief
    workroom carries a **Request approval** control (against the latest `plan` Dossier), renders
    `expired` distinctly from `rejected`, and shows a "bound to plan" cue. **Bound-plan approval now
    triggers decomposition — SHIPPED (backend + bridge; dashboard safe-response path; `execution
    §1.7/§1.8/§3.1`):** a new **`brief.plan_package_open`** capability creates, atomically, a *plan
    package* — an immutable `plan` Dossier + a `suggest_tasks` proposal + an approval-bound `confirm`
    linked to **both** (the new nullable `bound_interaction_id` column carries the proposal link). A
    companion **`brief.plan_confirm_respond`** answers that confirm: **accept** re-checks the plan is
    still latest and then **materializes the linked proposal exactly once through the resumable
    `brief_decomposition_claims` ledger** (assignee hints pre-validated through the assign-Key gate;
    duplicate accept idempotent → same ids), **reject** closes the confirm and its still-open
    proposal. Bridge routes `POST /v1/spine/briefs/:id/plan-package` + `…/plan-confirms/:cid/respond`
    and boot-policy allow rules/coverage shipped; the workroom routes a plan-package confirm (one
    carrying `bound_interaction_id`) through the safe response path so **Yes** triggers decomposition
    exactly once. *Still deferred:* full issue **document authoring / per-doc revision-locking /
    forking** (`execution §1.8`), a dashboard plan-package **editor** (only the safe response path
    ships; the open route exists), and wiring this into an **autonomous (LLM) planner** flow (no
    agent auto-authors the plan or auto-fires it). (The `owner`-liveness takeover gap is now **closed** — see the
    owner-takeover note above; for these synchronous operator interactions the honest model is
    operator-resumable with stale-age takeover, not a heartbeat-backed live run.)

---

## 4. Do Not Build Yet / Deferred (so future prompts don't wander)

These are **intentionally** out of scope right now. Do not start them without an explicit
instruction and a doc update.

- **Hermes rich seam** — `hermes_rig` is a stdio placeholder; real `/v1/runs` over Hermes,
  MCP gated tools, `relix-bridge` plugin, PerToolCall governance = future
  (`relix-hermes-integration.md`; ledger "Hermes rich seam" = NOT STARTED). **Gated on the
  open licensing question (§8.1).**
- **Tether plugin-hook system** — in-process lifecycle-hook bridge is unbuilt; the current
  plugin host is an out-of-process capability provider (ledger "Tether" = NOT STARTED).
- **Sandboxed Cell (container/VM isolation)** — required before Macro/Rig execution is
  exposed broadly; not yet built.
- **Full VCS merge** — run review/apply is inspect-and-copy; `git_worktree`/`git_checkout`
  workspace context and true merge are deferred (`current-limitations.md`; only `empty` and
  `copy_repo` ship).
- **Cloud sandbox / serverless Bench backends** — Hermes Phase H4; local-only for now.
- **Persistent Keeper / Bench backends** — Tradecraft/Keeper run behind in-memory ledgers.
- **DHT/gossip peer discovery, manifest signing, CRL/revocation, federated audit** —
  alpha-deferred mesh-hardening (SIMP-002/003/007/017; `current-limitations.md`).
- **Provider-ToS-dependent subscription posture** — running Max/ChatGPT subscriptions
  headlessly through the orchestrator is an open commercial question
  (`relix-agent-adapters.md §9.3`); keep behaviour honest, don't lean on it commercially.

---

## 5. Next 10 Work Slices (in order)

Each slice = one green, doc-conformant, pushable commit. Pick the top undone one.

1. **Claim 409 + per-agent start lock + same-Operative duplicate-start guard** —
   `execution-and-issue-design.md §1.4/§7.1/§2.6`.
   **✅ DONE.** *Files changed:* `crates/relix-runtime/src/nodes/coordinator/mod.rs` (per-Operative
   start-lock registry + `agent_start_lock`; the read-only `live_run_by_agent(brief, agent)`
   duplicate-start signal — live Claim by that Operative **and** running-run evidence; tests),
   `…/coordinator/heartbeat.rs` (acquire the start lock across the claim+commit in `preflight_run`
   **and**, before claiming, refuse `already_running` when `live_run_by_agent` shows a live run;
   manual-path conflict + concurrent-start + sequential/concurrent same-Operative duplicate-start
   tests), `crates/relix-web-bridge/src/spine.rs` (`run_report_response`/`json_with_status`: a
   Claim conflict `already_running` → `409 Conflict` carrying the structured `RunReport`; real +
   precondition statuses stay `200`; tests — **unchanged this slice**, the new refusal reuses the
   same `already_running` status the bridge already maps to 409). *Why the guard:* the start lock
   only serializes the critical section; `claim_brief_for_run` deliberately lets the same Operative
   refresh a live Claim (wakeup/heartbeat idempotency) and `preflight_run` mints a new run id — so
   without the guard two same-Operative starts would both open run rows/workspaces. The guard lives
   only in the start path, so the lower-level idempotent API is untouched, and it never blocks a
   continuation after a run finishes (Claim released + run terminal). *Pinned:* "never retry a 409";
   "first Ready/running, second refused `already_running`, no second run row/workspace". *Verified:*
   targeted + full `cargo test -p relix-runtime` green (3938 lib tests); `cargo check` clean;
   `cargo clippy` clean on the touched code (pre-existing warnings only, unrelated files);
   `git diff --check` clean. *Remaining of this Claim line → slice 10 (stale-run adoption by
   terminal evidence).*

2. **Guild-level spend hard-stop (autonomous)** — `company-model.md §6/§6.6`.
   **✅ DONE.** *Files changed:* `crates/relix-runtime/src/nodes/coordinator/heartbeat.rs`
   (the pure `guild_allowance_admits` verdict; `BudgetAdmission::Refuse` now carries the
   Chronicle `event` + refused-run `status` so a Guild stop reads `guild.budget_refused` /
   `over_guild_budget` and a per-Operative stop reads `brief.budget_refused` /
   `over_allowance`; `dispatch_budget_admits` composing per-Operative-then-Guild,
   tenant-safe; the dispatch path uses the carried event/status; tests),
   `crates/relix-runtime/src/controller_runtime.rs` (the live heartbeat `admit_budget`
   closure now calls `dispatch_budget_admits` with the SpineStore + metrics + the Brief's
   own Guild), `crates/relix-runtime/src/nodes/coordinator/mod.rs` (`TaskStore::task_tenant`
   made `pub` so the gate resolves a Brief's Guild without leaking another tenant's spend),
   `crates/relix-runtime/src/nodes/coordinator/agent/action_center.rs` (the "Guild spend over
   budget" card copy now states the autonomous dispatch gate refuses; manual runs sovereign).
   *Adds:* a Guild-cap gate on the autonomous path mirroring the per-Operative hard-stop and
   **additive** to it (per-Operative enforcement unchanged + authoritative); honest distinct
   event (`guild.budget_refused`). *Why additive + precedence:* the per-Operative gate bounds
   one Operative, the Guild gate bounds the whole Guild's autonomous spend so a fleet of
   in-budget Operatives can't collectively overrun the company ceiling; a per-Operative
   refusal takes precedence and is never weakened. *Tenant isolation:* the Guild spend is the
   sum of the Brief's OWN Guild's active Operatives' `cost_since` over the canonical Allowance
   window (now the current UTC calendar month, slice 9; trailing-30-day at the time of this
   slice) — never a cross-tenant `cost_since(None, …)` — the same figure + window the Action
   Center reports.
   *Pinned:* over-Guild-budget autonomous Brief refused + parked + chronicled as
   `guild.budget_refused`; under-budget / no-budget allowed; per-Operative stop takes
   precedence; cross-tenant spend does not trip another Guild's cap; manual `preflight_run`
   stays sovereign for the same over-budget Brief. *Verified:* full `cargo test -p
   relix-runtime` green (3944 lib tests, +6); `cargo check` clean; `cargo clippy` clean on the
   touched code (2 pre-existing unrelated warnings only); `git diff --check` clean. *(The
   issue-tree cost rollup + billing-code attribution backend shipped in §P1 slice 3b; the
   calendar-month spend window with implicit reset shipped in slice 9 = DONE; the
   delegation-depth counter + guard shipped in §P1 slice 3c; object-level
   (Mandate/Campaign/Guild) billing codes shipped in §P1 slice 3b. Remaining
   deferred: the frontend Costs surface.)*

3. **The Lattice org-chart view** — `dashboard-design.md §9`.
   **✅ DONE (partial).** *Files changed:* new `apps/dashboard/src/pages/Lattice.tsx`,
   `apps/dashboard/src/App.tsx` (route `/lattice`), `apps/dashboard/src/components/nav.ts`
   (ORG entry) + `Layout.tsx` (title), `apps/dashboard/src/styles.css` (lattice stage/node/
   edge/zoom styles), rebuilt `crates/relix-web-bridge/dashboard-dist`. *Adds:* a live SVG-edge
   + node-card `reports_to` tree from `/v1/spine/operatives` (apex order from
   `/v1/spine/company`), role/status/rig chips, direct-report counts, a live pill from
   `/v1/runs`, click → per-Operative Keys/allowance/risk-ceiling detail; B&W aesthetic (§12).
   *Partial:* full drag-pan/pinch **deferred** — ships a scrollable stage (overflow:auto = pan)
   + explicit −/reset/+ zoom controls (CSP-clean, no SVG-pan dependency). *Verify:* `npm run
   build` green; dist rebuilt + committed (dist-parity gate); `git diff --check` clean.

4. **Costs surface** — `dashboard-design.md §10`.
   **✅ DONE.** *Files changed:* new `apps/dashboard/src/pages/Costs.tsx`,
   `apps/dashboard/src/api.ts` (typed `briefCost.rollup` + `guildSpend.get` clients), `App.tsx`
   (route `/costs`), `nav.ts` (ORG entry) + `Layout.tsx` (title), rebuilt `dashboard-dist`.
   *Adds:* Guild budget vs **canonical month-to-date spend** (`guild.spend` →
   `GET /v1/spine/guild/spend`), per-Operative allowance (Keys) + observed spend
   (`/v1/metrics/agents`, 24h/7d/30d window), the Brief-tree rollup (own/descendant + per-
   billing-code breakdown), and budget/over-cap incident cards. All real data; honest
   unavailable states (route + reason). *Caveat closed (slice 11):* the canonical Guild MTD
   spend now has a numeric route — the Guild budget card reads it, not the metrics
   approximation. *Verify:* `npm run build` green; dist rebuilt + committed; `git diff --check`
   clean.

11. **Canonical Guild month-to-date spend route + Costs wiring** — `company-model.md §6/§6.6`,
    `dashboard-design.md §10`.
    **✅ DONE.** *Files changed:* `crates/relix-runtime/src/nodes/coordinator/heartbeat.rs`
    (extracted the shared `guild_spend_micros` helper — the single source of truth for
    "Guild month-to-date spend" — and refactored `dispatch_budget_admits` to call it),
    `…/coordinator/agent/handlers.rs` (new `handle_guild_spend` + 4 tests),
    `crates/relix-runtime/src/controller_runtime.rs` (register `guild.spend`, wired to the same
    `MetricsQuery` the Action Center uses), `crates/relix-web-bridge/src/spine.rs` +
    `…/main.rs` (route `GET /v1/spine/guild/spend`), `scripts/relix-mesh-up.{ps1,sh}` +
    `scripts/check-boot-policy-coverage.ps1` (`guild.spend` allow rule + manifest),
    `apps/dashboard/src/api.ts` (`guildSpend` client + `GuildSpend` type),
    `apps/dashboard/src/pages/Costs.tsx` (Guild budget card reads canonical spend), rebuilt
    `dashboard-dist`. *Adds:* one numeric route returning the Guild's actual current-UTC-month
    spend — the EXACT ledger figure + window the autonomous Guild hard-stop enforces (so the
    card can never disagree with the gate), with `spent_micros`/`spent_cents`,
    `budget_cents`/`remaining_cents`/`over_budget` (honest-null when no budget),
    `window_start_ms`/`resets_at_ms`/`now_ms`, and `source`/`computed_from`. Tenant-safe: sums
    ONLY the caller's own Guild's active Operatives (never `cost_since(None, …)`); no metrics
    ledger → null spend (never a faked 0). *Pinned:* current-month-only (stale row excluded),
    over-budget + negative remaining, no-budget honest-null, no-metrics null, tenant isolation.
    *Verified:* `cargo test -p relix-runtime --lib` green (3970, +4); `cargo test -p
    relix-web-bridge` green; `cargo clippy` clean on the touched code; `npm run build` green;
    dist rebuilt + committed (parity gate); boot-policy coverage PASS; `git diff --check` clean.

5. **Run transcript renderer (nice/raw)** — `dashboard-design.md §8`.
   **✅ DONE.** *Files changed:* new `apps/dashboard/src/components/RunTranscript.tsx`
   (reusable block-grouping renderer), `apps/dashboard/src/api.ts` (shared `RunEvent` type +
   `runControls.events`), `apps/dashboard/src/pages/Runs.tsx` (uses `<RunTranscript>` in the
   expanded run; dropped the flat per-event dump + local event state/loader), `styles.css`
   (`.xtr-*` B&W transcript blocks), rebuilt `dashboard-dist`. *Adds:* folds the real
   `/v1/runs/:id/events` stream into typed blocks — lifecycle rail, assistant/result message
   cards, **collapsible** grouped tool actions, permission-denied + error/stderr callouts, a
   usage/cost chip — with a **nice↔raw** segmented toggle (raw = compact verbatim dump). Live-
   tails the selected run via the existing run-event SSE (`subscribeRunEvents`) while it is
   `running`, with an honest live/reconnecting/**polling** chip and a 4s polling fallback when
   the stream is unavailable. Color is semantic-only; no fabricated cards. *Verify:* `npm run
   build` green; dist rebuilt + committed (parity gate); `git diff --check` clean.

6. **Streamed Brief thread + interaction cards** — `dashboard-design.md §7/§8`.
   **✅ DONE (partial — honest).** *Files changed:* `apps/dashboard/src/components/BriefDetail.tsx`
   (embeds `<RunTranscript>` as a **Live work** block in the Latest-Shift section so the agent's
   run is visible inside the workroom, not only on the Runs page; a `txKey` re-fetches it after a
   Shift mutation), rebuilt `dashboard-dist`. *Adds:* the active/latest run's transcript streams
   inline in the Brief; interaction cards already **refresh on the run-event SSE** because
   BriefDetail's existing subscription calls `reload()` (which refetches `interactions`) on any
   execution transition for this Brief. Existing answer/accept/reject controls and the
   invalidation-bus wiring are preserved unchanged. *Partial (honest gap):* there is **no
   dedicated interaction-card SSE** — a card raised by an agent **without** an accompanying run
   transition appears on the next run event or a manual Refresh, not instantly. The transcript
   itself is keyed by Brief (not run) on the stream, so it refetches on any transition while the
   Shift is `running`. *Verify:* `npm run build` green; dist rebuilt + committed; `git diff
   --check` clean.

7. **Approvals hub** — `dashboard-design.md §10`.
   **✅ DONE (partial).** *Files changed:* new `apps/dashboard/src/pages/Approvals.tsx`,
   `nav.ts` (+`/approvals` entry → auto-listed in the ⌘K palette), `App.tsx` (route),
   `Layout.tsx` (title + pending-Clearance nav badge), `api.ts` (`clearances.list/decide`,
   `companyActions.list`). Reads `/v1/spine/clearances` (the unified `coord.approval.pending`
   queue) and the `hire`/`budget` items of `/v1/spine/company/actions`; decides Clearances via
   `/v1/spine/clearances/:id/decide` and direct hires via `/v1/agents/:id/approve-hire|reject-hire`,
   then invalidates actions/mandates/briefs. *Honest gaps:* no per-type typed-payload editor (one
   generic approve/reject); budget alerts link to their own route (no inline decide route exists).
   *Verify:* `npm run build` green; dist rebuilt + committed.

8. **Settings hub** — `dashboard-design.md §10`.
   **✅ DONE (partial).** *Files changed:* `apps/dashboard/src/pages/Settings.tsx` (+`api.ts`
   `runtimeState.get/reset`). Added an **Admin · session recovery** section over
   `/v1/runs/runtime-state` (per-agent lookup of the persisted adapter runtime rows + a typed-confirm
   reset) on top of the already-real Health, Maintenance & storage, AI providers, run-execution
   sandbox, autonomous-heartbeat, Bridge-info, and Adapter-readiness sections. *Honest gap:*
   runtime-state is per-agent (the route requires an `agent_id`) — there is no global session list.
   *Verify:* `npm run build` green; dist rebuilt + committed.

9. **Allowance calendar-month windowing + reset bookkeeping** — `company-model.md §6/§6.6`.
   **✅ DONE.** *Files changed:* `crates/relix-runtime/src/nodes/coordinator/heartbeat.rs`
   (new canonical `allowance_window(now_ms) -> AllowanceWindow { start_ms, cutoff_ms,
   resets_at_ms }` = the current **UTC calendar month**, inclusive month start →
   next-month reset edge, with zero-dep Hinnant `civil_from_days`/`days_from_civil`;
   `dispatch_budget_admits` now derives `since_ms` from it instead of `now − 30d`; the
   month-boundary/leap/Dec→Jan reset test; the metrics-seeded budget tests pin their rows to
   the window start so they're deterministic at a month boundary),
   `crates/relix-runtime/src/nodes/coordinator/agent/handlers.rs`
   (`MetricsSpendSource::trailing_30d` → `current_month`, window from `allowance_window`;
   real-ledger live-spend tests seed relative to the window),
   `crates/relix-runtime/src/controller_runtime.rs` (call-site rename + comments),
   `crates/relix-runtime/src/nodes/coordinator/agent/action_center.rs` (operator copy
   "last 30 days" → "this month"; doc + test assertion). *Adds:* one canonical
   calendar-month window both the dispatch gate and the Action Center read, so they can never
   disagree; reset is implicit (spend re-summed from the live month start — no stored
   counter to clear); `resets_at_ms` is the bookkeeping value the surface can show. *Why
   UTC:* the mesh has no per-Guild billing timezone; a single stable zone keeps gate + feed +
   tests in agreement, and a future per-Guild zone changes only that one function. *Pinned:*
   window opens at the inclusive month start, resets at the next month's first instant; 1ms
   before the boundary belongs to the previous month; Feb 2024 is 29 days; December rolls
   into the next January. *Verified:* targeted + full `cargo test -p relix-runtime` green;
   `cargo check`/`cargo clippy` clean on the touched code. *(The issue-tree cost rollup +
   billing-code attribution backend shipped in §P1 slice 3b, the delegation-depth counter +
   guard in §P1 slice 3c, object-level (Mandate/Campaign/Guild) billing codes in §P1 slice 3b;
   remaining deferred: the frontend Costs surface.)*

10. **Stale-run adoption by terminal evidence** — `execution-and-issue-design.md §1.4/§7.1`.
    **✅ DONE.** *Files changed:* `crates/relix-runtime/src/nodes/coordinator/mod.rs` (new
    `TaskStore::reclaim_terminal_claim` + 4 store tests), `…/coordinator/heartbeat.rs`
    (`preflight_run` calls it after the duplicate-start guard, before `claim_brief_for_run`
    + 2 preflight tests). *Adds:* a dangling **live** Claim whose run pointer
    (`execution_run_id`, else `checkout_run_id`) references an already-**terminal**
    `brief_runs` row is reclaimed at start time — beyond the age-based `recover_stale_runs`
    → `interrupted` sweep, which only touches `running` rows and so never frees a Claim whose
    run is already terminal. *Safe by construction:* never releases a Claim that still backs
    a `running` run, a Claim whose pointer matches no run row for this Brief (no terminal
    evidence → never steal another actor's live Claim on a guess), or a **newer** Claim that
    re-acquired the Brief (conditional `UPDATE` keyed on the Claim's own pointer + holder).
    On a real reclaim it promotes the oldest deferred wakeup and records a
    `brief.claim_reclaimed` Chronicle note (only on the abnormal dangling case — no noise on
    normal completion). *Preserves slice 1:* a still-`running` matching run still refuses
    `already_running` → 409 (reclaim is a no-op on it); a terminal matching run now lets a new
    start proceed. *Tests:* store — releases on terminal pointer (+ chronicle + idempotent),
    leaves a `running` run alone, needs evidence matching the Claim's own pointer, does not
    clobber a newer running Claim; `preflight_run` — adopts a stale terminal Claim and a fresh
    start succeeds (one live run row), refuses `already_running`/409 (never retry) when
    another worker's run is still `running`. *Verified:* full `cargo test -p relix-runtime`
    green (3950 lib tests, +6); `cargo check` clean; `cargo clippy` clean on the touched code
    (2 pre-existing unrelated warnings in `maintenance.rs`); `git diff --check` clean.
    *Remaining edge (deferred):* Relix releases+re-claims rather than transferring the dead
    owner's checkout context in place (full Paperclip "adopt the prior checkout run"); the
    reclaim is wired into the manual/Prime start chokepoint, while the autonomous heartbeat
    path still relies on the age-based sweep for the same condition.

12. **Supervisory auto-wake promotion (blockers-resolved + children-completed)** —
    `execution-and-issue-design.md §1.6/§3.1`.
    **✅ DONE.** *Files changed:* `crates/relix-runtime/src/nodes/coordinator/mod.rs`
    (`set_board_status` now fires at the central terminal-transition seam:
    `promote_blockers_resolved` on entering `done`, `promote_children_completed` on entering
    `done` OR `cancelled`; the shared `offer_supervisory_wake` enqueue helper; the
    `all_subbriefs_terminal_in_tenant` readiness check; 7 store tests). *Adds:* first-class,
    event-driven follow-up sequencing — no busy-poll. A `done` Brief offers a wakeup to every
    **same-Guild** dependent that was `blocked_on` it; the shared `request_brief_wakeup` enqueue
    applies the readiness guard, so a dependent still waiting on ANOTHER unfinished blocker is
    `skipped` (not woken) and a now-fully-unblocked dependent is `queued`. A `done`/`cancelled`
    child offers a wakeup to each **same-Guild** parent once ALL its same-Guild Sub-briefs are
    terminal. Stable reasons `blockers-resolved` / `children-completed`, source `automation`.
    *Why the seam:* every done/cancel path (manual `brief.move`, the apply-driven
    `complete_reviewed_brief`, board recovery) flows through `set_board_status`, so the promotion
    lives once, not per UI route; the board lock is released before enqueuing (the enqueue locks
    the connection itself). *Tenant isolation:* only same-Guild dependents/parents are
    enumerated (`list_blocking_for_tenant` / `parent_briefs_for_tenant`) and only same-Guild
    Sub-briefs counted — a cross-Guild edge can neither wake nor leak another Guild's Brief.
    *Honest semantics:* only `done` resolves a blocker (a `cancelled` blocker keeps the
    dependent blocked, LOCKED §1.6); a `cancelled` child DOES count as terminal for the parent
    continuation wake (matching `list_briefs_with_all_children_done`); a missing assignee invents
    no one — it records a `brief.wakeup_skipped` Chronicle note. *No duplicate runs:* a repeated
    terminal transition coalesces into the live/queued wake. *Pinned:* blocker done wakes a
    fully-unblocked dependent; a second unfinished blocker holds the wake until it too is done;
    child completion wakes the parent only when all same-Guild children are terminal (incl. a
    cancelled last child); a missing assignee records an event but no wakeup; a cross-Guild edge
    does not wake/leak; a repeated done transition does not duplicate. *Verified:* targeted
    `auto_wake_*` (7) green; full `cargo test -p relix-runtime --lib` green (3977 lib tests, +7);
    `cargo check -p relix-runtime` clean; `cargo clippy` clean on the touched code (2 pre-existing
    unrelated warnings in `maintenance.rs`); `git diff --check` clean. *Now also shipped (partial):*
    exactly-once plan decomposition (§1.7 — durable `brief_decomposition_claims` ledger:
    fingerprint + `created_ids` resume cursor + crash-safe resume / no-op duplicate / no-fork
    accept + orphan-free concurrent accept via a per-decomposition materialization lock; see §P3
    slice 10). **Approval-bound plan *confirm* is now backend-shipped (first slice, §1.8):**
    `brief.plan_confirm_open` binds a `confirm` to the latest `plan` Dossier revision; a stale accept
    (after a newer plan revision or a superseding comment) expires the card and never resolves as
    approved. **Bound-plan approval now triggers decomposition (§1.7/§1.8/§3.1, backend + bridge):**
    `brief.plan_package_open` links an approval-bound confirm to a `plan` Dossier **and** a
    `suggest_tasks` proposal (new `bound_interaction_id` column); `brief.plan_confirm_respond` accept
    re-checks the plan is latest then materializes the linked proposal exactly once through the
    resumable ledger (idempotent duplicate accept; reject closes both). *Still deferred:* full issue
    document authoring / revision-locking / forking (§1.8), a dashboard plan-package editor (only the
    safe response path ships), and an autonomous LLM planner.

> After completing a slice: re-open the cited section, update the implementation map /
> divergence ledger in `product-spine-implementation.md`, and update this file's §2/§3 so
> the next run starts honest.

---

## 6. Definition of Done for "Product Feel"

Relix *feels like a real product, not a mock-up*, when all of these are true (from
`company-model §8.6–8.7` and `dashboard-design §12`):

- **Time-to-first-success < 5 min** — a fresh user boots, logs in, and watches a Brief reach
  `done` without reading docs (live-smoke already proves the path exists; it must be
  *discoverable in the UI*, not just via HTTP).
- **The org is visible** — the Lattice shows the company as a company; the Roster shows Keys
  and Allowance per Operative.
- **Work reads as a goal-facing plan** — Briefs render as numbered workflow checklists with
  sub-brief nesting and progress, not a flat log.
- **Live, not polled** — a running Shift shows a pulsing Live indicator and a streaming
  transcript in the Brief thread; the Desk updates without a manual refresh.
- **Cost is legible** — every Brief/Operative/Guild shows spend vs Allowance; over-cap is a
  visible incident, never a silent stop.
- **No silent failures** — every refusal/failure has a plain-language reason and a
  one-click recovery route (Action Center recovery cards already do this — extend to all
  surfaces).
- **B&W, dense, keyboard-first** — true-black/white, color for meaning only, ⌘K palette,
  skeletons not spinners, optimistic edits with rollback.
- **Honest** — nothing in the UI claims a capability the backend doesn't enforce; the
  divergence ledger has no undocumented gaps.

---

## 7. Prompting Rule for Future Claude / Codex Runs (BINDING)

Before writing any code in this repo:

1. **Read the Paperclip audit sources when doing product-direction work**:
   `references/paperclip/RELIX_PAPERCLIP_AUDIT_LOG.md`,
   `references/paperclip/.relix-audit/paperclip-file-line-coverage-summary.md`,
   `references/paperclip/.relix-audit/paperclip-file-line-coverage-progress.md`, and
   `docs/hermes-vs-paperclip-vs-relix.md`. Do not treat the six Relix docs alone as
   the whole product compass; they are Claude-authored adaptations of the Paperclip audit,
   not a replacement for it.
2. **Read the relevant Relix design-doc section next** and state it up front:
   *Section* (`<doc> §<n>`), *Files changed*, *Not changed / out of scope*.
3. **Then read this roadmap** (§2 for what exists, §3/§5 for what's next, §4 for what's
   deferred). Do not build anything in §4 without an explicit instruction + doc update.
4. **Build exactly what the section specifies** — no invented features, no unrequested
   layout/IA/naming changes. The lexicon is binding on product surfaces.
5. **Work only on `main`.** No branches, no history rewrite, no force-push. Author stays
   `Anshul Raman <ramanal@mail.uc.edu>`, no AI attribution. Stage with explicit paths.
6. **Commit + push each green, doc-conformant slice**, citing the design-doc section in the
   message (the established convention — see the git log).
7. **No fake UI or fake data.** Every surface reads real backend routes; if a route is
   missing, build it or surface the gap — don't mock it.
8. **After every change:** re-open the cited section, verify conformance, run `cargo test`
   (touched crate then workspace) + `cargo clippy` on touched crates, rebuild
   `dashboard-dist` if `apps/dashboard` changed (dist-parity gate), and **update the
   divergence ledger + this roadmap** so the next run starts from the truth.
