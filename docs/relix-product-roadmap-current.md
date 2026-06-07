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
> `3c3c533c`, Live Spend Telemetry Pack era), the Paperclip audit files listed below, and
> the design docs listed below.

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
  the run start path now returns **HTTP `409`** (never a retryable `200`), and an in-process
  **per-Operative start lock** serializes concurrent starts (§1.4/§2.6). *Still partial:*
  stale-run adoption by terminal evidence (see gaps / §5 slice 10).
- **Entry guards** — `in_progress` requires assignee + no unresolved Snags; `in_review`
  requires a real reviewer.
- **Brief detail API** — `brief.detail` returns the full product object (fields,
  sub-briefs, parents, snags, dossiers, labels, due, claim, `latest_run`, chronicle).
- **Thread interactions** — answerable `ask` / `confirm` / `suggest_tasks` cards
  (`brief_interactions` table), with governed assignee hints, backward-only `after`
  dependencies, idempotent accept, children inheriting parent context.
- **Desk / Inbox reads** — `/v1/spine/inbox`, `/v1/spine/briefs/:id/thread`,
  `/v1/spine/unassigned`; board cards surface unresolved same-Guild blockers.

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
  (trailing-30-day, best-effort); `brief.budget_refused` event.
- **Tenant isolation** — product agent/governance routes are tenant-scoped; a known id from
  another Guild resolves not-found.
- **Chronicle** — hash-chained events for every run transition, interaction, and Prime
  action; **durable activity ledger** `/v1/activity/recent` (`bridge-activity.jsonl`).
- **Live spend telemetry** — Action Center surfaces real trailing-30d spend vs Allowance
  through the gate's own `MetricsSpendSource` (commit `579fa8c5`, ledger-reconciled in
  `177c93ef`).

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
1. **[BE] Claim stale-run adoption by terminal evidence** — the `409` conflict surface and
   the per-Operative start lock **shipped** (roadmap §5 slice 1: `brief.run` maps a Claim
   conflict `already_running` → HTTP `409`, never a retryable `200`; an in-process
   per-Operative start lock serializes concurrent starts; "never retry a 409" pinned in
   tests). What remains of the two-pointer Claim is **stale-run *adoption by terminal
   evidence*** — see §5 slice 10 (`execution §1.4`/`§7.1` LOCKED; ledger "Claim HTTP 409 +
   per-Operative start lock" = DONE, "stale-run adoption" = PARTIAL).
2. **[BE] Guild-level spend hard-stop** — only per-Operative Allowance is enforced; the
   Guild cap is **alert-only** today (`company-model §6.6`; ledger "Operative Allowance" &
   "Action Center" = PARTIAL). Manual `brief.run` is intentionally *not* Allowance-gated
   (operator sovereign) — documented, but the Guild ceiling should still bind autonomous spend.
3. **[BE] Allowance windowing** — trailing-30-day approximates the doc's calendar-month
   window; no reset bookkeeping, no issue-tree cost rollup, no billing-code attribution.

**P2 — product-feel surfaces (mostly frontend on data that already exists)**
4. **[FE] The Lattice (org chart)** — pan/zoom org-tree view is **not started**
   (`dashboard-design §9`; ledger "Missing surfaces"). The Roster (Agents.tsx) exists; the
   *visual org* that sells "a company" does not.
5. **[FE] Full Costs surface** — spend by Guild/Operative/Campaign/Brief with budget
   progress + incident cards + tree rollup (`dashboard-design §10`). Data exists (metrics +
   Allowance); there is no dedicated Costs page.
6. **[FE] Run transcript renderer** — block-grouped "nice"/"raw" transcript view, live-tailed
   (`dashboard-design §8`). SSE + `run_events` exist; the rich renderer does not.
7. **[FE] Streaming Brief thread** — the workroom is request/response; the design wants the
   thread to merge **live** run transcript + **streamed** interaction cards via one socket
   (`dashboard-design §7/§8`; ledger "Brief workroom interactions" = static cards only).
8. **[FE] Approvals + Settings hubs** — full Approvals surface with spend-enforcement
   controls and a real Settings hub (`dashboard-design §10`; Settings.tsx is a stub).

**P3 — depth / autonomy**
9. **[BE/FE] Smarter companion** — `prime.propose` AI mode is opt-in and rule-validated;
   replacing the deterministic planner with an LLM driving the governed spine APIs is
   future (`current-limitations.md`; ledger "Mandate orchestration" still not autonomous).
10. **[BE] Exactly-once decomposition + cost-tree rollup + auto-wake promotion** — deferred
    (`execution §1.7`; ledger line 209 = NOT STARTED).

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

1. **Claim 409 + per-agent start lock** — `execution-and-issue-design.md §1.4/§7.1/§2.6`.
   **✅ DONE.** *Files changed:* `crates/relix-runtime/src/nodes/coordinator/mod.rs` (per-Operative
   start-lock registry + `agent_start_lock` + tests), `…/coordinator/heartbeat.rs` (acquire the
   start lock across the claim+commit in `preflight_run`; manual-path conflict + concurrent-start
   tests), `crates/relix-web-bridge/src/spine.rs` (`run_report_response`/`json_with_status`: a
   Claim conflict `already_running` → `409 Conflict` carrying the structured `RunReport`; real +
   precondition statuses stay `200`; tests). *Pinned:* "never retry a 409" in test names/comments.
   *Verified:* targeted + full `cargo test` on both touched crates green; `cargo check`/`clippy`
   clean for the changes; `git diff --check` clean. *Remaining of this Claim line → slice 10
   (stale-run adoption by terminal evidence).*

2. **Guild-level spend hard-stop (autonomous)** — `company-model.md §6.6`.
   *Files:* `action_center.rs`, heartbeat dispatch in coordinator, `spine.rs`. *Adds:* a
   Guild-cap gate on the autonomous path mirroring the per-Operative hard-stop; honest event
   (`guild.budget_refused`). *Test:* over-Guild-cap autonomous Brief is refused; manual run
   stays sovereign. *Verify:* `cargo test`; ledger entry updated PARTIAL→DONE-for-autonomous.

3. **The Lattice org-chart view** — `dashboard-design.md §9`.
   *Files:* new `apps/dashboard/src/pages/Lattice.tsx` (or extend Agents.tsx), `nav.ts`;
   reads `company.status` reports-to tree. *Adds:* pan/zoom SVG tree, click → Operative
   detail; B&W aesthetic (§12). *Verify:* rebuild `dashboard-dist` (dist-parity gate),
   live-smoke shell loads the view, screenshot.

4. **Costs surface** — `dashboard-design.md §10`.
   *Files:* new `apps/dashboard/src/pages/Costs.tsx`, `nav.ts`; reads metrics + Allowance
   (the same source the Action Center uses). *Adds:* spend by Guild/Operative/Campaign,
   budget progress bars, over-cap incident cards. *Verify:* rebuild dist; manual against a
   smoke mesh with a completed Shift.

5. **Run transcript renderer (nice/raw)** — `dashboard-design.md §8`.
   *Files:* `apps/dashboard/src/pages/Runs.tsx` + new transcript component; reads
   `run_events` + SSE (`subscribeRunEvents`). *Adds:* block grouping (assistant/tool/diff/
   event), nice↔raw toggle, live tail. *Verify:* rebuild dist; live Claude/echo run shows a
   readable transcript.

6. **Streamed Brief thread + interaction cards** — `dashboard-design.md §7/§8`.
   *Files:* `BriefDetail.tsx`, `api.ts` (SSE subscription), `invalidate.ts`. *Adds:* live
   run transcript inline in the Conversation; interaction cards refresh on stream, not just
   on open. *Verify:* rebuild dist; ledger "Brief workroom" static→streaming.

7. **Approvals hub** — `dashboard-design.md §10`.
   *Files:* new `apps/dashboard/src/pages/Approvals.tsx`, `nav.ts`; reads
   `coord.approval.pending`/`get`, decides via `coord.approval.decide`. *Adds:* typed
   payload detail (hire/strategy/budget/high-risk), inline greenlight/reject. *Verify:*
   rebuild dist; approve a starter-crew hire from the hub in a smoke mesh.

8. **Settings hub** — `dashboard-design.md §10`.
   *Files:* flesh out `apps/dashboard/src/pages/Settings.tsx`; surfaces existing config
   (admin recovery pointers, maintenance, run-workspace context mode, theme). *Adds:* a real
   settings home, not a stub. *Verify:* rebuild dist; manual.

9. **Allowance calendar-month windowing + reset bookkeeping** — `company-model.md §6`.
   *Files:* metrics/allowance enforcer in coordinator, `action_center.rs`. *Adds:*
   calendar-month window with reset, replacing trailing-30d approximation; near-band
   configurable. *Test:* month-boundary reset test. *Verify:* `cargo test`; ledger updated.

10. **Stale-run adoption by terminal evidence** — `execution-and-issue-design.md §1.4`.
    *Files:* `recover_stale_runs` + claim store. *Adds:* a dead checkout reclaimed when
    terminal evidence proves the prior Shift ended (beyond the current age-based
    `interrupted` sweep). *Test:* adoption test with a stale checkout + terminal run row.
    *Verify:* `cargo test`; ledger "Claim two-pointer" PARTIAL detail closed.

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
