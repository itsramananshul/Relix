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
- **Company operations summary** (company-model §5.4/§8.2; dashboard-design §5) —
  `company.status` (`GET /v1/spine/company`) now also carries a read-only, tenant-scoped
  `operations` object so the Overview reads as ONE coherent company snapshot instead of
  stitching the agent/identity dimension to separately-composed Brief/run/Mandate counts.
  It derives ONLY from existing tenant-scoped store reads (the same helpers the Action
  Center uses, so the snapshot and the feed can never disagree and never fabricate a figure):
  `briefs` (total + `by_board` buckets + `in_review` / `ready_to_start` / `unassigned` /
  `blocked` / `stale`), `runs` (a bounded recent `window` classified into `running` /
  `failed_or_refused` / `pending_review`), `approvals` (`pending_clearances` /
  `pending_hires`), and `mandates` (total + `by_status` + `strategy_proposed`). Backward-
  compatible (the base initialized/founder/prime/crew fields are unchanged; the summary is
  additive), best-effort (a transient sub-read degrades that bucket to `0`, never failing the
  core read), and read-only (no new authority/route/policy). The Overview surfaces it as a
  flat **Operations snapshot** card in the cockpit, alongside the intact Action Center +
  Company operating status card.
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
- **Prime guided driver v1** (company-model §5.4/§8.2 + §12.5/§12.5B) — `prime.next_step`
  (READ-ONLY) classifies the ONE next governed step for a Prime proposal or a Mandate over
  live state (approval → strategy gate → team plan + live readiness → Brief board → run
  ledger), with a `phase` / `label` / `reason` / `route` / `can_advance` / `advance_action`.
  `prime.advance` runs **at most one** safe, explicitly-requested step
  (`create_team_plan` — plan from the Mandate's existing active crew, adopts active
  Operatives + mints **no** hires; or `orchestrate_assign_ready` — existing
  `mandate.orchestrate` in `assign_ready` mode), re-reading state and **refusing as stale
  (HTTP 409, no side effects)** when the requested action is no longer current, through the
  same governed handler + Keys as the manual route. It is **not** self-approving: no
  blind loop, no approval action inside the manual one-step driver, never runs a real
  adapter (Start stays the explicit button), one click = one step. The separate
  autonomous standing-authority layer can approve only the explicitly granted
  proposal/strategy/hire/spawn-Clearance categories; budget is never delegated. Bridge:
  `GET /v1/spine/{prime/proposals,mandates}/:id/next-step` +
  `POST …/advance`; dashboard: the Chat Shift Room **and the Overview cockpit**
  ("Company operating status") show the next step + a restrained **Advance one step** button
  when `can_advance`, else the route to take by hand. The Overview cockpit picks the most
  relevant active object (latest Prime proposal, else the latest Mandate via the twin route),
  pairs the step with board/run counts from the payload + a live pressure strip from the
  Action Center, is best-effort (a missing next step never blanks the Overview), and on a
  stale `409` shows an honest banner + reloads the fresh step. Still one safe explicit step,
  **not** a self-approving strategy loop.

### Briefs / Workroom (`relix-execution-and-issue-design.md` §1, §1.9)
- **Two-pointer Claim** — `checkout_run` + `execution_run`, self-refresh, lease/release,
  lock clearing on assignee/state change (the LOCKED model, §7.1). A Claim **conflict** on
  the run start path now returns **HTTP `409`** (never a retryable `200`), an in-process
  **per-Operative start lock** serializes concurrent starts, and a **same-Operative
  duplicate-start guard** refuses a *new* start (`already_running` → `409`) when that
  Operative already has a live, actually-running run on the same Brief — so a double-start
  can no longer open two run rows/workspaces while the lower-level `claim_brief_for_run`
  stays idempotent for wakeup/heartbeat/recovery (§1.4/§2.6). **Stale-run adoption by
  terminal evidence** now ships on **both** the manual/Prime and the **autonomous
  heartbeat** paths (§5 slice 10): a dangling **live** Claim whose run pointer
  (`execution_run_id`/`checkout_run_id`) points at an already-**terminal** `brief_runs` row
  is reclaimed via the shared `reclaim_terminal_claim` helper — called in `preflight_run`
  before the manual/Prime claim, and as a batch admission step
  (`reclaim_terminal_claims_ready`) at the **top of every heartbeat dispatch tick** — so a
  start *or* an autonomous re-dispatch proceeds on terminal evidence instead of waiting for
  the age-based `recover_stale_runs` sweep / lease expiry. Safe by construction (never
  releases a Claim backing a still-`running` run, a Claim with no matching run evidence, or
  a newer Claim), idempotent, tenant-safe, and chronicled `brief.claim_reclaimed`.
  *Remaining edge:* Relix releases+re-claims rather than transferring the dead owner's
  checkout context in place (full Paperclip "adopt the prior checkout run").
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
- **Work-object IA** — Overview, Briefs (**Board / Plan toggle** — kanban with drag-drop +
  contextual detail panel, and a goal-facing numbered workflow checklist; see the Briefs Plan
  view slice below),
  Agents (Roster + per-Operative Keys panel), Mandates (governed strategy→orchestrate
  workflow), Runs, Chat (Prime), Company, Settings, Scheduled.
- **Action Center / The Desk** — `GET /v1/spine/company/actions`: ranked next-actions with
  severity chips, plain-language reasons, recovery-decision cards (root cause → one route),
  refreshed off SSE + low-frequency poll.
- **Overview cockpit ("Company operating status")** — surfaces the Prime guided-driver's ONE
  next safe step for the most relevant active object (latest proposal, else latest Mandate),
  with the payload's board/run counts, a live Action-Center pressure strip (approvals / hires /
  budget / recovery / review / ready), and a restrained **Advance one step** button when
  `can_advance` (else the route to take by hand). Best-effort + honest empty/stale states; uses
  the existing `prime.next_step` / `prime.advance` routes — no new backend authority. Still one
  explicit governed step; the separate opt-in autonomous Prime driver can now
  run those same approved-work steps on a bounded timer — and, with **Manual
  Autonomy Tick v1**, an operator can also **Run Prime now** (`prime.autonomy_tick_now`
  / `POST /v1/spine/prime/autonomy/tick`, Settings) to fire **exactly one** bounded
  tick for their Guild and read back the per-candidate tick records (considered /
  advanced / started). The background runtime toggle controls the *timer*; this
  tick-now wake-up is an explicit operator action for one Guild that does **not**
  require the timer to be ON, is operator/admin-only + tenant-scoped, and still
  obeys standing grants, the autonomous-start budget hard-stop, Rig readiness, and
  the per-tick `RELIX_AUTONOMOUS_PRIME_MAX` bound (it grants no new authority).
- **Operations snapshot** — a compact, read-only cockpit card fed by `company.operations`
  (the server-computed, tenant-scoped summary above): three glance groups — *work in flight*
  (running / ready / in review), *needs attention* (unassigned / blocked / stale / recovery),
  and *governance* (pending approvals / hires / strategy / mandates) — each stat a deep link to
  where it's worked. Honest unavailable state when the summary is absent (the agent-only
  fallback read); no nested cards; the Action Center + Company operating status card stay intact.
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
   last open piece, **stale-run *adoption by terminal evidence***, now shipped on **both**
   the manual/Prime and the autonomous-heartbeat paths (§5 slice 10): a dangling **live**
   Claim whose run pointer references an already-**terminal** `brief_runs` row is reclaimed
   via the shared `reclaim_terminal_claim` helper — at start time (`preflight_run`) and as a
   batch admission step (`reclaim_terminal_claims_ready`) at the top of every heartbeat
   dispatch tick — safe by construction, idempotent, tenant-safe, and chronicled
   `brief.claim_reclaimed`, so a start *or* an autonomous re-dispatch proceeds on terminal
   evidence without waiting for the age-based `recover_stale_runs` sweep / lease expiry
   (`execution §1.4`/`§7.1` LOCKED; ledger "Claim HTTP 409 + per-Operative start lock +
   duplicate-start guard" = DONE, "stale-run adoption" = DONE, "heartbeat stale-claim
   admission" = DONE). **Heartbeat-origin Claims are now adoptable too:** the autonomous
   claim path mints ONE durable `run_` id at wakeup-queue time and carries it through, so a
   heartbeat-claimed Brief's Claim pointer (`execution_run_id`) IS the `brief_runs.run_id`
   the dispatcher records — closing the old `shift_?`-pointer / `run_?`-ledger asymmetry that
   meant a heartbeat-origin Claim's pointer could never match a terminal run. A reclaim also
   closes the now-id-aligned mid-flight wake, so the adopted Brief re-dispatches the same
   tick with no duplicate run/wake. *Remaining edge (deferred):* Relix releases+re-claims
   rather than transferring the dead owner's checkout context in place (full Paperclip "adopt
   the prior checkout run"); and adoption still requires the Claim's pointer to match a
   recorded terminal `brief_runs` row, so the only Claim freed by lease expiry instead is one
   lost in the narrow window before its run row was ever written (a crash between the claim
   and `record_run_start`).
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
4. **[FE] The Lattice (org chart)** — **FRONTEND SHIPPED** (`dashboard-design §9`).
   `apps/dashboard/src/pages/Lattice.tsx` (nav `/lattice`) renders the live `reports_to`
   forest from `/v1/spine/operatives` (+ `/v1/spine/company` for apex order) as an SVG-edge +
   node-card tree, role/title/status/rig chips + direct-report counts, a live pill driven by
   `/v1/runs`, and click → a per-Operative detail (Keys + allowance + risk ceiling via
   `/v1/spine/keys/:id` + `/v1/agents/:id`, now with a clickable direct-reports list to walk
   the tree). **Exact Operative deep links now ship across the Lattice + Crew (Agents):** a
   selected Lattice node's detail panel links **Govern on Crew →** to `/agents?agent=<id>`, and
   the Crew page is URL-driven (`/agents?agent=<id>`) — opening that Operative's governance
   panel automatically, highlighting + scrolling its row/card into view, with View/Hide and a
   Copy-link affordance writing the query param so refresh/back/forward preserve the selection
   (an unknown id renders an honest "no Operative matches …" banner, not a crash). **The
   per-Operative detail is now a full tabbed WORKBENCH (`dashboard-design §9`), not a row
   expansion** (Crew/Operative detail slice): selecting an Operative (any Open button, a Lattice
   deep link, or `/agents?agent=<id>`) opens a prominent employee-record panel with seven tabs —
   **Overview** (role/status/adapter/readiness, reports-to + clickable direct-reports, pressure
   summary, open assigned Briefs from the board fetch, recent Shifts), **Instructions** (the
   Operative's charter / instruction bundle from the full-profile `agent.keys` read — now **viewable
   and editable**: view mode shows it as bounded, scrollable preformatted text — never parsed as
   HTML — with a char/line summary and an honest empty state when none is stored; **Edit** opens a
   bounded textarea and **Save** writes through the configure-gated `PATCH /v1/agents/:id
   { instruction_bundle }`, where an empty draft **clears** the charter and Cancel restores the last
   loaded value), **Skills** (the Operative's **procedural memory** — reusable recipes relevant to it,
   surfaced **read-only** from the existing `memory.skill_*` catalogue via
   `GET /v1/skills?agent=<id>&q=<query>&limit=20`: each entry shows name + status/version/confidence/
   usage + a bounded description preview + source/tags/id, with an Enter/Search-only filter, and honest
   unavailable / empty states; **no create/update/deprecate or per-Operative skill-assignment UI** —
   read-only by design, tab-gated so it only fetches while open), **Permissions** (the intact
   Keys + capability-powers + standing-approvals governance face — nothing removed), **Runs**
   (this Operative's Shifts from the existing `/v1/runs` payload — status/trigger/rig/duration/
   started + deep links to `/runs?run=<id>`), **Budget** (committed Allowance / risk ceiling /
   concurrency / approval timeout, explicitly labelled *committed* not spent, with live spend
   routed to the Costs page — never fabricated), and **Configuration** (adapter + autonomy/
   heartbeat flags + org placement + identity + timestamps from `/v1/agents/:id`; the charter is
   linked to the Instructions tab as viewable/editable, Skills to the Skills tab as read-only, and a
   per-Operative **model preference** (model name + reasoning/effort) is now viewable + editable here and
   consumed by supported subscription CLI adapters). The tab is URL-driven (`&tab=<tab>`, default Overview, unknown value falls back
   safely) and Copy-link captures it; all tabs but Skills reuse the page's existing fetches (the shared
   detail cache + `/v1/runs` + the board columns), and the Skills tab adds one bounded, tab-gated fetch
   to the **existing** `/v1/skills` route — **no new backend route and no extra fetch loop**. *Remaining gap:* by design this stays the inline workbench on the Crew page (NOT a
   separate `/agents/:id` router route — the query-param deep link is canonical); the charter /
   instruction-bundle is now **viewable and editable** on the Instructions tab (read via `agent.keys`,
   written via the configure-gated `PATCH /v1/agents/:id { instruction_bundle }` — this writes the
   instruction bundle only, NOT a config-history UI), the **Skills** tab is now built (read-only over the
   existing `/v1/skills` — no per-Operative skill-assignment / create-update-deprecate editor), and the
   per-Operative **model lane** is now built as a real adapter preference (model name + reasoning/effort,
   editable via the configure-gated `PATCH /v1/agents/:id`) and execution consumes it through
   `RigRunRequest::model_preference` / `reasoning_effort`: the dispatch chokepoint threads the assigned
   Operative's stored preferences into manual, heartbeat, Prime-start, and guarded-retry runs; Claude maps
   the model to `--model`, Codex maps the model to `--model` and effort to
   `-c model_reasoning_effort=<tier>`, and unsupported rigs ignore the fields. **Full pan/zoom/pinch + Fit/Reset now ship** (the previously-deferred gap is
   closed): the stage is one CSS `transform: translate() scale()` viewport driven by native
   PointerEvent (drag-pan + two-finger pinch) and a non-passive WheelEvent (cursor-anchored
   zoom), with explicit −/+/Fit/Reset controls, an auto-fit-once on first render (no jump on
   hover/click), and `touch-action:none` for mobile gesture handling — still CSP-clean and
   dependency-free (no SVG-pan lib). Cycles + orphan `reports_to` pointers render safely
   (visited-set DFS; an orphan/cycle node falls back to a root, an edge to a missing parent is
   simply not drawn). *View state now persists:* the viewport (scale + pan x/y) is saved to
   `localStorage` under the versioned key `relix.lattice.viewport.v1` (debounced, hard-validated
   on read — finite numbers, scale within range, pan within a sane bound; corrupt values reset
   cleanly). Fit persists the fitted view, Reset overwrites with the default, and a restored
   viewport suppresses the one-time auto-fit so it is never clobbered. *Honest remaining nuance:*
   `brief.detail`'s `delegation_depth` is still not rendered here.
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
7. **[FE/BE] Streaming Brief thread** — **SHIPPED** (`dashboard-design §7/§8/§11`).
   The Brief workroom embeds the live run transcript inline (`<RunTranscript>` Live-work block),
   and a **dedicated interaction-card SSE** now refreshes the ask/confirm/suggest/plan-package
   cards on its own: `GET /v1/spine/briefs/:id/interactions/stream` (bridge `interactions_stream`)
   tenant-scopes exactly like the list route by proxying the same `brief.interactions` capability,
   emits an initial snapshot, then pushes `event: interactions` only when the card list's
   fingerprint changes (terminal `event: not_found` on an unknown/cross-Guild Brief; transient
   `event: error` keeps trying). The detail subscribes via `subscribeBriefInteractions` and updates
   the cards directly (§11 surgical update), with a subtle "live" cue on the Requests header; the
   run-event SSE `reload()` still owns the rest of the workroom, so a card raised **without** a run
   transition now surfaces within the poll window instead of only on the next run event / manual
   refresh. *Honest caveat:* this is **polling-backed SSE** (~2.5s, fingerprint-gated) like the
   Prime status stream — not a true backend event source / full websocket push.
8. **[FE] Approvals + Settings hubs** — **FRONTEND SHIPPED (partial)** (`dashboard-design §10`).
   New `/approvals` page + nav/palette entry: pending **Clearances** from `/v1/spine/clearances`
   (unified `coord.approval.pending` queue — spawn-hire/strategy/budget/high-risk, decided inline
   via `/v1/spine/clearances/:id/decide`), plus direct **pending hires** + **budget alerts** from
   `/v1/spine/company/actions` (hire approve/reject via `/v1/agents/:id/approve-hire|reject-hire`).
   A pending-Clearance nav badge; decisions invalidate the actions/mandates/briefs surfaces.
   Settings hub gains an **Admin · session recovery** section that now **auto-loads the global
   runtime-state list** (`GET /v1/runs/runtime-state/list`, capability `rig.runtime_state.list`):
   a tenant-scoped table of **every** persisted adapter session in the Guild (Operative, Rig, Brief,
   masked session, status, tokens, cost, updated), a client-side filter (Operative/Rig/Brief/status/
   session fragment), and a per-row guarded reset (brief-scoped reset for a row with a `brief_key`;
   typed `RESET` confirm for the dangerous agent-level reset). On top of the existing
   Health/Maintenance/Adapter/run-sandbox/heartbeat sections. The Approvals hub is now **typed**:
   the bridge preserves the runtime approval row's typed fields (`subject_id` / `capability_category`
   / `expires_at` / `task_id`) through `/v1/spine/clearances`, and the page groups Clearances by type
   (**Hire/Spawn · Strategy · Budget/Allowance · High-risk/Other**) with a per-type payload summary
   (requesting actor, affected subject, capability category, age/expiry, parked-Brief target route),
   a filter/search bar, and a per-group decision-impact line. High-risk/strategy/budget decisions
   require a short operator note (typed confirmation); hire approvals stay fast. *Honest gaps:* the
   budget-alert decision still routes out to Costs/Operatives (**no inline budget-decision route
   exists** — labelled by kind: spend alert vs committed-Allowance plan vs hard-stop, never a fake
   "decide"); the typed summary is limited to the fields the runtime records (no free-form
   resource/scope/payload editor, because the runtime stores none); Rig binding at approval is only
   available for **direct** hires (the spawn-Clearance decide cannot set a Rig — the card says so and
   routes to the Operative page); the stored session id is surfaced only as a **masked/truncated**
   summary, and session **resume is still stored-not-replayed**. The **per-SESSION reset** still has no diagnosis of its own (it
   forgets the row) — but the separate, now-shipped **run-level** Brief/Shift diagnosis layer (§P1
   slice 3d) does classify retryable-vs-not on the durable `brief_runs` ledger.
3d. **[BE/FE] Brief/Shift recovery diagnosis (v1)** — **SHIPPED** (`execution-and-issue §3.3b`,
   `dashboard-design §5.2/§8`). Every terminal or refused `brief_runs` Shift is stamped with a pure,
   derived recovery diagnosis: additive `failure_class` / `retryable` / `retry_budget_remaining` /
   `recovery_action` / `recovery_route` columns, a stable `failure_class` bucket (`precondition` /
   `governance` / `budget` / `adapter_unavailable` / `workspace` / `timeout` / `cancelled` /
   `interrupted` / `transient` / `permanent` / `unknown`), a true retryable-vs-not verdict
   (timeout/transient Rig failure → retryable; governance / permanent / auth / config / tool-permission
   failure + every refusal → not retryable), a **small operator-facing** retry budget (0 or 1, NOT an
   auto-retry counter), and a recommended action + dashboard route. Pure classifiers
   (`RunDiagnosis::for_terminal` / `for_refusal`) are stamped at the run chokepoints
   (`record_run_finish` + the dispatch finalize re-stamp with the real `RigOutcome` retryable signal;
   `record_refused_run`); surfaced on `RunRecord` / `brief.runs` / the Brief detail `latest_run`; the
   Action Center `failed_or_refused` card prefers the durable metadata (falling back to the refusal
   map) and rides failure-class + retryable + budget badges; the Runs page shows a recovery strip.
   `over_guild_budget` is now a durable refused row too. *Honest scope:* diagnosis + operator guidance
   ONLY — **no autonomous retry orchestration**, **no blind auto-retry loop**, **no provider quota
   polling**, and **no fake retry button** (it points at the EXISTING governed route). The task-level
   `task.retry` recovery is a separate, unchanged layer.
3e. **[BE/FE] Guarded operator Shift retry (Stage-2, bounded)** — **SHIPPED** (`execution-and-issue
   §3.3b` Stage-2b *Retry now*). Turns the Stage-1 retryable verdict into a real **one-click**
   operator recovery — **NOT** a blind auto-retry loop and **NOT** a company planner. Additive
   `brief_runs.retried_from_run_id` / `retry_attempt` lineage + a partial UNIQUE index enforcing
   at-most-one child per source (the duplicate guard). New capability `run.retry` + route
   `POST /v1/runs/:run_id/retry`: a pure tenant-scoped `retry_precheck` refuses unless the source is
   terminal-and-failure-like (`failed`/`interrupted`), `retryable`, has budget, and links a
   still-present in-tenant Brief with no existing child; eligible runs open **exactly one** child
   through the SAME `preflight_run` path as `brief.run` (shared adapter/Claim/workspace/ledger/
   governance) and stamp the lineage + chronicle `brief.retry_requested` / `retry_started`. Honest
   HTTP mapping: started/`already_retried` → 200 (idempotent, returns the existing child), claim
   conflict → 409, precondition refusal → 400, not-found/cross-tenant → 404 (no leak). The Runs page
   shows a **Retry Shift** button only when eligible (+ lineage links); the **Action Center recovery
   card now carries the exact source run id + the safe retry action where possible**, so the operator
   can retry from the cockpit — `failed_item` emits `run_id` + `action_api = POST /v1/runs/<run_id>/retry`
   only when the run mirrors `retry_precheck` eligibility (failed/interrupted + retryable + budget) AND
   the handler sees no existing retry child (`retried_sources`, like the Runs page), so it never offers
   a retry the route would refuse; `Overview.tsx` renders a compact **Retry Shift** button beside the
   intact inspect-in-Runs link and follows the child run on success (links the existing child on
   `already_retried`). *Honest scope:* operator-triggered only — **no blind auto-retry** (every retry is
   one explicit click through the governed re-checking route), **no autonomous retry on the heartbeat
   path**, **no LLM diagnostic pass / Inbox card** (Block/Reassign/Investigate not built), **no provider
   quota polling**.

**P3 — depth / autonomy**
9b. **[BE/FE] Prime guided driver v1** — **SHIPPED (bounded one-step guide, NOT self-approving).**
   Closes part of the long-standing "the Prime/company flow is governed but not a driver" gap honestly.
   `prime.next_step` (READ-ONLY) names the ONE next governed step for a proposal/Mandate from live
   state; `prime.advance` runs **one** safe, explicitly-requested step (`propose_strategy` /
   `create_team_plan` / `orchestrate_assign_ready`) through the existing gated handler, re-reading state
   and **refusing as stale (409) with no side effects** on mismatch. The manual one-step driver performs
   no approval action, **never** runs a real adapter (Start stays the explicit button), and
   is **not** a blind loop — one click advances one step. The autonomous standing-authority layer can approve
   only the explicitly granted proposal/strategy/hire/spawn-Clearance categories; budget is never delegated.
   **Prime Strategy Drafting v1 — SHIPPED:** a
   Mandate with no strategy yet is classified `needs_strategy_proposal` and the driver (manual click or
   the opt-in autonomous tick) can **DRAFT** a strategy doc and propose it through the
   existing `mandate.strategy.propose` path — *draft only*, left `proposed` for a human to approve, never
   overwriting an existing proposed/approved/rejected strategy (so a rejection is honoured). The body is
   deterministic by default and **opt-in model-authored** under `RELIX_PRIME_LLM_STRATEGY_DRAFT` for the
   autonomous/manual-tick loop (Prime Strategy Authoring v1, below). *Files:*
   `crates/relix-runtime/src/nodes/coordinator/agent/prime_driver.rs`
   (+ `controller_runtime.rs` registration, `handlers.rs` reuse), `crates/relix-web-bridge/src/{spine.rs,main.rs}`
   (4 routes + 409 mapping), `apps/dashboard/src/{api.ts,pages/Chat.tsx}`, the boot scripts +
   coverage manifest, rebuilt `dashboard-dist`. *Update:* strategy **approval** is no longer always
   human — the bounded standing-authority layer adds a `prime.strategy.approve` category, so when the
   autonomous Prime loop is effectively ON **and** the Board has granted that standing authority for the
   Guild, the loop approves a *proposed* strategy through the existing `mandate.strategy.approve` handler
   (a separate per-candidate action from drafting; tenant-scoped, bounded, consumes one grant call; a
   **rejected/missing** strategy is never approved or re-proposed). With **no grant** strategy approval
   stays human exactly as before. **Bare-Mandate autonomous start — SHIPPED:** a **bare Mandate** (one
   reached `ready_to_start` with **no** owning Prime proposal) now has its ready same-tenant Briefs
   **started by the loop itself** (action `start_mandate`) — not left to the heartbeat / manual `brief.run`
   — through the **same shared guarded run pipeline** the heartbeat dispatcher and `prime.start` use
   (`heartbeat::preflight_and_spawn_with_trigger` → `preflight_run_with_prefs_trigger` → `prepare_claimed_run`
   → `execute_ready`): claims, duplicate-run guard, live adapter probe, scoped workspace prep, durable
   `brief_runs` ledger, bridge-token minting, board advancement, Chronicle (`prime.autonomous_mandate_start`).
   No second run system is invented — the run is stamped as an **autonomous/heartbeat** trigger (not dashboard
   `manual`); the ready set is tenant-scoped (`list_ready_briefs_for_tenant`, filtered to the Mandate, no
   cross-Guild leak); the **same autonomous budget hard-stop** (`dispatch_budget_admits` per ready Brief)
   blocks the whole start with **zero** runs if any is over budget; an already-claimed/running Brief is never
   double-started; and it counts exactly one tick action when ≥1 run starts. Starting is **not** an approval
   gate, so this needs **no** standing grant. **Prime Deliberation v1 — SHIPPED (opt-in, default OFF):** the
   autonomous loop is no longer a hardcoded deterministic state machine — behind
   `RELIX_PRIME_LLM_DELIBERATION` a model may **CHOOSE among the already-computed governed actions** for a
   candidate (confirm the single legal next action, or HOLD `none` this tick), but **the model is not the
   permission system**: a strict server-side validator (`prime_deliberation::parse_prime_decision`) bounds its
   choice to `[<computed action>, none]`, every confirmed action still flows through the same governed handler +
   standing authority + budget + Claim + adapter + tenant gates, and any malformed/disallowed/unavailable output
   falls back deterministically with an honest `ai_mode` (`deterministic_only`/`llm_used`/`fallback`/`unavailable`,
   surfaced on `prime.autonomy_tick_now`). The live decider performs only the existing `ai.chat` mesh call to the
   AI peer (`RELIX_PRIME_AI_PEER`/`RELIX_PRIME_LLM_SESSION`) — **no provider key in the coordinator / web bridge /
   dashboard**. Both the background timer **and** the manual **Run Prime now** tick (`prime.autonomy_tick_now`)
   build the SAME `MeshAiDecider` from the coordinator's populated outbound mesh client, so the manual tick
   exercises live deliberation when the mesh AI peer is reachable (the controller runs the tick from a blocking
   thread so the decider's `Handle::block_on` never runs on an async worker); when the mesh cell is unpopulated or
   the peer is unreachable it honestly falls back to `unavailable` + deterministic. *Files:*
   `crates/relix-runtime/src/nodes/coordinator/agent/prime_deliberation.rs` (new pure
   module + 16 tests), `…/agent/prime_driver.rs` (wrapper + `MeshAiDecider` + manual-tick helper + loop tests),
   `…/agent/mod.rs`, `controller_runtime.rs` (live wiring for the timer + the manual tick),
   `apps/dashboard/src/pages/Settings.tsx` + rebuilt `dashboard-dist`.
   **Prime Strategy Authoring v1 — SHIPPED (opt-in, default OFF):** behind `RELIX_PRIME_LLM_STRATEGY_DRAFT`,
   when the autonomous/manual-tick loop executes `propose_strategy` and a live mesh decider is available, a
   model authors the *body* of the PROPOSED strategy from a bounded, secret-free snapshot (Mandate
   title/status/description + active roles + readiness counts); the reply is re-validated + sanitized
   server-side (`prime_strategy::validate_strategy_draft` — rejects empty/over-long/injection, sanitizes the
   pipe + control chars, appends a "DRAFT / not approved" footer, bounds to `STRATEGY_DRAFT_BODY_CAP`) and is
   only ever **proposed** — the human `mandate.strategy.approve` gate is unchanged and an existing
   proposed/approved/rejected strategy is never overwritten. Unavailable/malformed/disabled output falls back
   to the deterministic draft with honest provenance (`strategy_ai_mode`/`strategy_ai_reason`, distinct from
   the action-choice `ai_mode`). It reuses the deliberation layer's `MeshAiDecider`/AI peer/session — **no
   provider key in the coordinator / web bridge / dashboard.** The explicit one-click `prime.advance` strategy
   route stays deterministic by design. *Files:*
   `…/agent/prime_strategy.rs` (new pure module + tests), `…/agent/prime_driver.rs`
   (`draft_strategy_doc` + record provenance + loop wiring + tests), `…/agent/mod.rs`,
   `…/spine/store.rs` (`strategy_doc` read accessor), `controller_runtime.rs` (both tick wiring sites),
   `apps/dashboard/src/pages/Settings.tsx` + rebuilt `dashboard-dist`.
   **Prime Executive Prioritization v1 — SHIPPED (opt-in, default OFF):** behind `RELIX_PRIME_LLM_PRIORITIZATION`,
   when the autonomous/manual-tick loop has ≥2 candidates carrying a positive **attemptable** action and a live
   mesh decider is available, a model chooses only the **order** in which the bounded tick spends its action
   budget across the already-computed legal candidates (or returns an empty order to **hold** the queue). The
   reply is validated to the offered candidate keys only (`prime_priority::parse_priority_order` — rejects
   unknown/duplicate keys, non-array/missing `order`, too-many-keys, non-string keys, malformed/over-long JSON or
   prose, over-long/control-char reason); invalid/unavailable/disabled output falls back to the byte-for-byte
   deterministic discovery order with honest provenance (`priority_ai_mode`/`priority_ai_reason`/`priority_rank`,
   distinct from `ai_mode`/`strategy_ai_mode`). **The model is NOT the permission system** — it only reorders
   (or holds) the deterministic classifier's attemptable menu and can never invent a candidate, add or widen an
   action, approve a gate it lacks a standing grant for, or bypass budget/Claim/adapter/tenant scope; each
   executed step flows through the SAME governed handler + gates. Closes the "candidate order is fixed-
   deterministic — with `MAX=1` the loop spends the tick on the first deterministic candidate even if another
   legal candidate is more important" gap. It reuses the deliberation layer's `MeshAiDecider`/AI peer/session —
   **no provider key in the coordinator / web bridge / dashboard.** *Files:* `…/agent/prime_priority.rs` (new
   pure module + tests), `…/agent/prime_driver.rs` (tick refactor + record provenance + loop wiring + tests),
   `…/agent/mod.rs`, `controller_runtime.rs` (both tick wiring sites), `apps/dashboard/src/pages/Settings.tsx` +
   rebuilt `dashboard-dist`.
   **Prime Orchestration Authoring v1 — SHIPPED (opt-in, default OFF):** behind `RELIX_PRIME_LLM_ORCHESTRATION`,
   when the autonomous/manual-tick loop executes `orchestrate_assign_ready` and a live mesh decider is available,
   a model authors the *text* — titles / dossiers / checklists — of the Brief skeleton the deterministic
   readiness logic has ALREADY computed (parent / role tracks / subject executions) from a bounded, secret-free
   snapshot (Mandate title/status, a bounded approved-strategy excerpt, the active role keys + staffed agent ids,
   gap roles + reasons, `max_briefs`). The reply is a VALIDATED blueprint keyed strictly by the offered
   role/subject keys (`prime_orchestration::parse_orchestration_blueprint` — rejects unknown top-level/role/subject
   keys, arrays where objects are expected, over-long/non-string title/dossier/checklist items, too many checklist
   items, malformed/over-long JSON or prose; sanitizes pipe→`/` + control chars) and passed (never raw model
   output) into `handle_orchestrate_with_blueprint`. **The model is NOT the permission system** — it authors text
   only and can never invent a role, agent, Brief id, source marker, dependency, assignee, approval, budget
   change, or tool; the roles/agents/assignments/reviewer stamping/`max_briefs` cap/placeholder behaviour/
   source-marker idempotency are byte-for-byte the deterministic path, a newly-created Brief gets the model text
   while an existing/hand-edited title is **never** clobbered (reuse is by source marker; titles set on creation
   only), and placeholder-track text stays deterministic. Invalid/unavailable/disabled output falls back to the
   deterministic titles + dossiers with honest provenance (`orchestration_ai_mode`/`orchestration_ai_reason`,
   distinct from `ai_mode`/`strategy_ai_mode`/`priority_ai_mode`). Closes the "the orchestrated Brief tree's text
   is mechanical/rule-based only" gap. It reuses the existing `MeshAiDecider`/AI peer/session — **no provider key
   in the coordinator / web bridge / dashboard** — and the **direct one-click** `mandate.orchestrate` /
   `prime.advance {action:"orchestrate_assign_ready"}` route stays deterministic by design. *Files:*
   `…/agent/prime_orchestration.rs` (new pure module + tests), `…/agent/handlers.rs`
   (`handle_orchestrate_with_blueprint` text integration), `…/agent/prime_driver.rs`
   (`author_orchestration_blueprint` + record provenance + loop wiring + tests), `…/agent/mod.rs`,
   `controller_runtime.rs` (both tick wiring sites), `apps/dashboard/src/pages/Settings.tsx` + rebuilt
   `dashboard-dist`.
   **Prime Shift Disposition v1 — SHIPPED (opt-in, default OFF, two SEPARATE grants):** closes the last
   autonomy seam at the end of a Shift — a completed run (`done` + `pending_review`) no longer waits on a human
   to accept and apply it. Two new standing-authority categories `prime.run.review_accept` (autonomously
   **accept** a completed Shift's review through the existing review path `TaskStore::set_run_review`) and
   `prime.run.apply` (autonomously **apply** an already-accepted run through the EXACT manual apply body
   `controller_runtime::execute_run_apply` — `run_apply_eligibility`, baseline-hash/conflict/artifact safety,
   and the review-to-done `complete_reviewed_brief`). **Both default OFF, separately grantable, never combined:**
   review and apply are distinct grants and distinct ticks (first tick accepts; next applies). Candidate
   selection is deterministic + tenant-scoped (`disposition_candidate` over the Mandate/proposal's OWN Brief
   set, oldest-first by `(started_at, run_id)`, apply before fresh accept, with a `run_belongs_to_tenant` guard
   so a cross-tenant run is **invisible** and an arbitrary Action Center run is never selected); eligibility is
   computed, never modelled (review = latest run `done`+`pending_review`; apply = `done`+`accepted`, apply
   status not already `applied`/`discarded`/`conflicted`/`failed`, `run_apply_eligibility` ok). A
   conflicted/failed apply records `blocked`, **never** marks the Brief done, consumes no grant, and is not
   retried in the tick. `review_accept`/`apply_run` are added to `KNOWN_ACTIONS` so the optional
   deliberation/prioritization layers may only confirm/hold/order them — the model never decides eligibility,
   invents a run id, bypasses a grant, or bypasses apply safety; a `none`/hold causes zero side effects. With
   neither grant, accept + apply stay a human's exactly as before. *Files:*
   `…/agent/prime_driver.rs` (categories, `disposition_candidate`, classifier phases, executor (B4), 8 tests),
   `…/agent/prime_deliberation.rs` (`KNOWN_ACTIONS`), `apps/dashboard/src/pages/Settings.tsx`
   (two category labels) + rebuilt `dashboard-dist`.
   *Still deferred:* a true end-to-end no-grant autonomous driver that also **approves** on its own (propose →
   **approve** → staff → orchestrate with nothing granted) — intentionally not built; **freeform tool-calling**
   remains deferred (deliberation only confirms-or-holds a computed action; prioritization only reorders the
   already-computed legal candidate queue; orchestration authoring only writes the *text* of the already-computed
   Brief skeleton; the model may now author a *proposed* strategy's body but never approves it, invents a goal, or
   calls a tool).
9. **[BE/FE] Smarter companion** — **BACKEND SHIPPED (now AI-assisted action selection, opt-in +
   validated + fallback; still one-turn / one-action, NOT autonomous).**
   The `POST /v1/spine/companion` parser is a **company-aware action spine**
   (`relix-dashboard-design.md` §13): beyond create/move/comment, it reads live company state in
   plain language — `what needs attention` → `company.actions` (ranked next actions),
   `what is blocked` → `brief.blocked_list`, `what is running` → `brief.runs` (active Shifts),
   `who is on the crew` → `agent.operatives` (roster) — replying with the top-3 titles / counts while
   keeping the raw JSON in `result`. It can also open a **governed plan package** from one line
   (`plan package <brief_id>: <body> => child: <t>; child high: <t>`) via `brief.plan_package_open`,
   refusing an empty body or zero children and **never** bypassing the approval-bound confirm
   (priorities only, no assignee hints). Every read/write goes through the SAME mesh capabilities +
   governance the dashboard uses. **New: an opt-in `mode:"ai"` adds model-assisted *action
   selection*** (mirroring the Prime planner seam): the bridge sends the AI peer a bounded,
   secret-redacted prompt (the operator message + a few company-context summary lines, never a JSON
   dump) and the model may ONLY return ONE strict-JSON action from a fixed allowlist. The bridge
   **validates** that choice into the existing `CompanionAction` enum — enforcing the same
   constraints as the parser (no unsafe pipes, valid board statuses, valid priorities, plan packages
   need a body + ≥1 child, no smuggled assignee hints) — and then runs it through the **exact same
   governed handler** as the deterministic path. The model never calls a tool, never has freeform
   text executed, and never picks a capability. On AI unavailable / invalid JSON / disallowed action
   / unsafe fields, it **falls back to the deterministic parser** with an honest `ai_mode`
   (`llm_used` / `fallback` / `unavailable`) + a safe reason, and the reply says so — it never fakes
   an AI success. The dashboard's "Use AI" checkbox now drives the Command button too. **Still one
   turn → one validated action; no autonomous planner/agent loop** (`current-limitations.md`; ledger
   autonomous Prime driver is shipped for approved-work orchestration, and bounded standing
   grants — including `prime.strategy.approve` — can delegate specific approvals; **model-reasoned**
   strategy/approval remains deferred, and with no grant approvals stay human).
   **Companion chat *console* now FRONTEND SHIPPED (product polish, `dashboard-design §13`):** the Chat
   page is a usable operating console, not a stateless command box — over the SAME governed routes (no
   new mutation path). It **persists the chat log locally** (versioned `localStorage` key `relix.chat.v1`,
   latest-50 cap, clean reset on a corrupt value, a confirmed **Clear chat**) so a refresh keeps the
   conversation — **browser-local UI history only, NOT the server/audit record** (governed actions still
   land in the Chronicle); adds a **command-chip row** (safe read chips fire directly; write templates
   only populate the input to edit first); renders a companion response as a **compact result card**
   (action chip + reply + honest AI-provenance chip + a safe route hand-off to the board + raw result
   behind a `<details>` disclosure) instead of a raw-JSON dump; and keeps the input muscle memory
   (**Enter = Plan with Prime**, **Ctrl/⌘+Enter = Command**). *Honest scope:* frontend only — no backend/
   capability/governance change, the persisted log is not synced, and it is still one turn → one
   validated action.
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
    exactly once. **Issue document authoring / per-doc revision-locking / forking is now BACKEND +
    DASHBOARD SHIPPED (v1, `execution §1.8`):** a new **`brief.dossier_author`** capability authors a
    Dossier revision with **optimistic concurrency** — additive nullable `author` /
    `revision_of_doc_id` / `forked_from_doc_id` columns on `task_documents` (a derived 1-based
    `revision_number` per Brief+kind in the read, so legacy / `brief.dossier_add` / plan-package rows
    get one too). `mode=revise` (default) writes the next linear revision; when the caller passes
    `expected_latest_doc_id` it MUST still equal the current latest of that kind, else the write is
    refused as **stale** (a typed `{stale:true,…}` result, **nothing written** — Dossiers stay
    immutable/append-only). `mode=fork` requires a `base_doc_id` on the **same Brief+kind** and writes
    a new append-only row carrying `forked_from_doc_id` **even if the latest moved** (the deliberate
    "branch from a stale/base revision" escape hatch, never an accidental overwrite). Every write
    Chronicles `brief.dossier_authored` / `_revised` / `_forked`; `brief.dossiers` / `dossier_get` /
    `dossier_latest` now carry the new metadata (existing clients unaffected). Bridge: a
    `POST /v1/spine/briefs/:id/dossiers/author` route maps a stale-lock refusal to an honest **`409`**
    (never a 502; "never retry a 409"), plus a `GET …/dossiers/latest?kind=` to load a body for
    editing; both behind boot-policy allow rules/coverage. Dashboard: the Brief workroom gains a
    compact **Documents** editor (kind/title/body textarea — no rich text) listing each kind's latest
    revision with `Edit latest` (loads under the optimistic lock), `Save revision` (a 409 keeps the
    draft + marks it stale), and `Fork from loaded revision`; saving a new `plan` revision naturally
    makes any plan-bound approval stale (the approval binds the latest plan id). *Still deferred:* a
    full rich-text editor / markdown renderer overhaul, a collaborative cursor, an external document
    store, and wiring this into an **autonomous (LLM) planner** flow (no agent auto-authors the plan
    or auto-fires it). The dashboard plan-package **composer** also ships (manual, not an editor).
    (The `owner`-liveness takeover gap is **closed** — see the owner-takeover note above; for these
    synchronous operator interactions the honest model is operator-resumable with stale-age takeover,
    not a heartbeat-backed live run.)

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
   *Follow-up shipped:* full drag-pan/pinch + Fit/Reset now close the earlier partial gap:
   PointerEvent drag-pan + two-finger pinch, cursor-anchored wheel zoom, auto-fit-once,
   and explicit zoom/Fit/Reset controls (CSP-clean, no SVG-pan dependency). *Follow-up
   shipped:* the viewport (scale + pan) now persists to `localStorage`
   (`relix.lattice.viewport.v1`, debounced + hard-validated on read), so a refresh/return
   restores the last view; Fit/Reset overwrite it consistently and a restored view suppresses
   the auto-fit. *Verify:* `npm run build`
   green; dist rebuilt + committed (dist-parity gate); `git diff --check` clean.

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

6. **Streamed Brief thread + interaction cards** — `dashboard-design.md §7/§8/§11`.
   **✅ DONE.** *Files changed:* `crates/relix-web-bridge/src/spine.rs` (new `interactions_stream`
   handler + pure `interactions_fingerprint` helper + unit test), `crates/relix-web-bridge/src/main.rs`
   (route `GET /v1/spine/briefs/:id/interactions/stream`; also added to the route-conflict test),
   `apps/dashboard/src/api.ts` (`subscribeBriefInteractions`), `apps/dashboard/src/components/BriefDetail.tsx`
   (embeds `<RunTranscript>` as a **Live work** block; subscribes to the interaction stream and
   updates cards directly with a subtle "live" cue), rebuilt `dashboard-dist`. *Adds:* the
   active/latest run's transcript streams inline in the Brief, AND a **dedicated interaction-card
   SSE** refreshes the ask/confirm/suggest/plan-package cards on its own — it tenant-scopes exactly
   like the list route by proxying `brief.interactions`, emits an initial snapshot, then pushes
   `event: interactions` only when the list's fingerprint changes (terminal `not_found` on an
   unknown/cross-Guild Brief). So a card raised **without** an accompanying run transition now
   surfaces within the poll window instead of only on the next run event / manual Refresh. The
   run-event SSE `reload()` still owns the rest of the workroom; existing answer/accept/reject
   controls and the invalidation-bus wiring are preserved. *Honest caveat:* the card stream is
   **polling-backed SSE** (~2.5s, fingerprint-gated) like the Prime status stream — not a true
   backend event source / full websocket push. *Verify:* `cargo test -p relix-web-bridge
   spine::tests` green; `npm run build` green; dist rebuilt + committed; `git diff --check` clean.

7. **Approvals hub** — `dashboard-design.md §10`.
   **✅ DONE (typed).** *Files changed:* `apps/dashboard/src/pages/Approvals.tsx` (typed hub),
   `api.ts` (`Clearance` typed fields), `crates/relix-web-bridge/src/spine.rs`
   (`parse_clearance_lines` preserves the typed columns + bridge tests),
   `crates/relix-runtime/src/nodes/coordinator/agent/handlers.rs` (`handle_approval_pending` appends
   the typed TSV columns + a runtime test). Earlier slices added `nav.ts`/`App.tsx`/`Layout.tsx`.
   Reads `/v1/spine/clearances` (the unified `coord.approval.pending` queue, now carrying
   `subject_id` / `capability_category` / `expires_at` / `task_id`) and the `hire`/`budget` items of
   `/v1/spine/company/actions`; **groups Clearances by type** (Hire/Spawn · Strategy ·
   Budget/Allowance · High-risk/Other) with a per-type payload summary, filter/search, and a decision-
   impact line; decides via `/v1/spine/clearances/:id/decide` (sensitive types require a note) and
   direct hires via `/v1/agents/:id/approve-hire|reject-hire`, then invalidates
   actions/mandates/briefs. *Honest gaps:* budget alerts route out to Costs/Operatives (no inline
   budget-decision route exists, so they are never labelled "decide"); the typed summary is limited to
   the runtime's recorded fields (no free-form resource/scope/payload editor); the spawn-Clearance
   decide cannot bind a Rig (only the direct-hire approve can). No new approval authority is created
   and nothing is auto-approved. *Verify:* `cargo test -p relix-web-bridge --bins spine::tests`
   + `relix-runtime` approval_pending tests green; `npm run build` green; dist rebuilt + committed.

   **Live Clearance stream (follow-up slice) — ✅ DONE.** *Files changed:*
   `crates/relix-web-bridge/src/spine.rs` (new `clearances_stream` handler + pure
   `clearances_fingerprint` helper + two unit tests), `crates/relix-web-bridge/src/main.rs`
   (route `GET /v1/spine/clearances/stream`; also added to the route-conflict test),
   `apps/dashboard/src/api.ts` (`subscribeClearances` + `ClearanceStreamConn`),
   `apps/dashboard/src/pages/Approvals.tsx` (subscribes on mount, renders from stream snapshots,
   header live/reconnecting/unavailable chip, bounded polling fallback), rebuilt `dashboard-dist`.
   *Adds:* the Approvals hub is now **live** — a dedicated SSE proxies the SAME
   `coord.approval.pending` capability the `…/clearances` list route serves (captured tenant
   re-applied per call — no new privilege, no cross-Guild leak), emits an initial
   `event: clearances` snapshot, then pushes again only when the parsed queue's fingerprint changes
   (a Clearance raised/decided/expired), with `event: error` on transient mesh blips while it keeps
   retrying. The existing Refresh button and the decide → invalidate → reload flow are unchanged
   (decisions still go through `/v1/spine/clearances/:id/decide`; the runtime cap owns
   authorisation). *Honest caveat:* this is **polling-backed SSE** (~2.5s, fingerprint-gated) like
   the interaction/Prime-status streams — NOT a true backend event bus / websocket; when the stream
   can't connect the page falls back to a bounded ~7s refresh; the company action feed (direct
   hires + budget alerts) still uses refresh/polling (no live backend source was fabricated for it).
   *Verify:* `cargo test -p relix-web-bridge --bins` green (772 incl. the new fingerprint + route
   tests); `cargo clippy -p relix-web-bridge` clean; `npm run build` green; dist rebuilt + committed.

8. **Settings hub** — `dashboard-design.md §10`.
   **✅ DONE (partial).** *Files changed:* `apps/dashboard/src/pages/Settings.tsx` (+`api.ts`
   `runtimeState.list/get/reset`). Added an **Admin · session recovery** section on top of the
   already-real Health, Maintenance & storage, AI providers, run-execution sandbox,
   autonomous-heartbeat, Bridge-info, and Adapter-readiness sections.
13. **Global runtime-state session list + control-plane recovery surface** —
    `dashboard-design.md §10`, `current-limitations.md`.
    **✅ DONE.** *Files changed:* `crates/relix-runtime/src/nodes/coordinator/mod.rs`
    (`TaskStore::list_runtime_state_for_tenant(tenant, limit)` — every persisted
    `agent_runtime_state` row for the tenant across ALL agents, `updated_at DESC` with stable
    `(agent_id, rig, brief_key)` tie-breakers, clamped to `MAX_RUNTIME_STATE_LIST = 200`; +2 store
    tests for cross-agent span / tenant isolation / limit-respect-and-clamp),
    `crates/relix-runtime/src/controller_runtime.rs` (new tenant-scoped capability
    `rig.runtime_state.list` returning `{rows:[...]}`, optional `{"limit":n}` arg, defaults on empty
    args, uses `ctx.tenant_id_or_default()`), `crates/relix-web-bridge/src/{main.rs,spine.rs}`
    (`GET /v1/runs/runtime-state/list[?limit=N]` → `runtime_state_list` proxy; the per-agent get +
    reset routes untouched), `scripts/relix-mesh-up.{ps1,sh}` + `scripts/check-boot-policy-coverage.ps1`
    (`rig.runtime_state.list` allow rule + manifest), `apps/dashboard/src/api.ts`
    (`runtimeState.list(limit?)` + `RuntimeStateRow` now carries `rig`/`last_run_id`/`last_status`/
    `last_error`/`input_tokens`/`output_tokens`/`cost_micros`),
    `apps/dashboard/src/pages/Settings.tsx` (the recovery panel now auto-loads the global list as a
    filterable table with per-row guarded reset; masked session id), rebuilt `dashboard-dist`. *Adds:*
    the operator can see and recover every adapter session in the Guild without first typing one agent
    id. Tenant-safe: the store filters by `tenant_id` exactly like the per-agent path; a foreign-Guild
    row never appears. *Honest remaining:* the stored session id is shown only as a **masked/truncated**
    summary; session **resume is still stored-not-replayed**; and the per-SESSION reset has no
    diagnosis of its own (it forgets the row). *(The separate **run-level** Brief/Shift diagnosis
    layer — failure-class/retryable/retry-budget on `brief_runs` — has since SHIPPED; see §P1 slice
    3d.)* *Verified:* targeted
    `cargo test -p relix-runtime --lib runtime_state` (5) green; `cargo test -p relix-web-bridge
    spine::tests --bins` green; `cargo check`/`cargo clippy` clean on the touched code; `npm run build`
    green; dist rebuilt + committed (parity gate); `git diff --check` clean.

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
    resumable ledger (idempotent duplicate accept; reject closes both). **The dashboard now has a
    minimal manual plan-package composer** (plan title/body + approval prompt + a child-task list with
    optional priority and an earlier-sibling `after` dependency) wired to `briefPlanConfirms.open`, so
    a human can open a plan package from the workroom; the created bound confirm is then approved
    through the already-safe response path. **Issue document authoring / per-doc revision-locking /
    forking is now shipped (v1, §1.8):** `brief.dossier_author` writes an append-only Dossier revision
    with optimistic concurrency (matching `expected_latest_doc_id` revises; a stale base is refused as
    a typed `{stale:true}` no-write; `mode=fork` branches from a base via `forked_from_doc_id`), a
    `POST …/dossiers/author` bridge route maps a stale lock to **409**, and the Brief workroom carries
    a compact **Documents** editor (kind/title/body textarea, latest-per-kind list, Edit-latest under
    the lock, Save-revision, Fork-from-loaded). *Still deferred:* a rich-text editor / markdown
    overhaul, a collaborative cursor, an external document store, and an autonomous LLM planner — the
    composer + editor are manual surfaces, not a full rich editor or LLM planner.

> After completing a slice: re-open the cited section, update the implementation map /
> divergence ledger in `product-spine-implementation.md`, and update this file's §2/§3 so
> the next run starts honest.

---

## 6. Definition of Done for "Product Feel"

Relix *feels like a real product, not a mock-up*, when all of these are true (from
`company-model §8.6–8.7` and `dashboard-design §12`):

- **Time-to-first-success < 5 min** — a fresh user boots, logs in, and watches a Brief reach
  `done` without reading docs (live-smoke already proves the path exists; it must be
  *discoverable in the UI*, not just via HTTP). **Shipped (frontend):** the **Overview**
  page now renders a compact **"Run your first Shift"** on-ramp for an initialized company
  with no work yet — one click chains the existing `company.starter_crew` (echo) →
  `brief.create` (assigned) → `brief.run` (echo) routes end-to-end and deep-links to the
  run/Brief/crew, so the safe-local first success is discoverable from the dashboard (no new
  backend; echo only, no real provider). See `product-spine-implementation.md` "Dashboard
  first-run on-ramp". *Still partial:* it proves the **local echo** loop only; the
  real-provider first-success path still routes through Settings + a coding-agent CLI.
- **The org is visible** — the Lattice shows the company as a company; the Roster shows Keys
  and Allowance per Operative.
- **Work reads as a goal-facing plan** — Briefs render as numbered workflow checklists with
  sub-brief nesting and progress, not a flat log. **Shipped (bounded):** the Briefs page has a
  **Board / Plan** toggle (`?view=plan`); Plan renders the loaded board cards as a numbered
  (`1`, `1.1`, …) checklist with parent/child nesting, status/priority/assignee/mandate chips,
  blocker chips, latest-run state + deep link, and a per-parent progress strip — all computed
  ONLY from the visible window (relation detail bounded-fetched for the first 80 cards; the
  rest render flat, labelled honestly). **Still partial:** no whole-tree (beyond the loaded
  board) rollup, no cross-route/realtime progress push, and relation detail is cached per
  session (a relation added mid-session shows on the next board reload of new ids).
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
