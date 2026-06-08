# Current Limitations

This document lists what the Relix alpha **does not do**, in plain terms,
without hedge or roadmap promises. The goal is that anyone evaluating
Relix can decide quickly whether the limitations are acceptable for
their use case.

Read this **before** deploying to anything other than a local developer
machine.

The corresponding "what does work" surface is in the
[README](../README.md). The corresponding "documented alpha trade-offs
with rationale and resolution gate" is in
[`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md);
where a limitation here corresponds to a SIMP entry there, it's cited.

## Operations and resilience

### Coordinator is a Task ledger, not a flow scheduler

The Coordinator node-type owns a durable SQLite ledger of Task records
+ per-attempt execution rows + events (see
[`coordination.md`](coordination.md) and
[`attempt-lineage.md`](attempt-lineage.md)). It does **not**:

- Watch for peer health.
- Auto-launch interrupted tasks. The C1b recovery scan promotes
  overdue `running` rows to `interrupted` and closes the open
  attempt — that's it. Re-launch is operator-driven.
- Schedule or queue work — there is no auto-scheduler picking up
  `pending` tasks.
- Auto-retry. The C2c `task.retry` capability validates state +
  budget and flips metadata; the operator (or the bridge on a fresh
  request) still has to actually re-run the flow.
- Resume a flow mid-execution (the SOL VM is synchronous — see
  [`replay-model.md`](replay-model.md) for the honest framing).

What it gives you is durable per-attempt records of who tried to do
what, the chronicle of how it went, and the pointer to where the
per-flow event log lives. Retry decisions are operator-driven via
`relix-cli task retry`.

### Adapter session recovery is a masked summary + reset, not session replay

The Settings hub now surfaces a **global** adapter-session recovery
table: `GET /v1/runs/runtime-state/list` returns every persisted
`agent_runtime_state` row for the current Guild across **all**
Operatives (newest first, tenant-scoped, clamped to 200), so an
operator can find and clear a wedged session without first knowing an
agent id. Each row shows the Operative, Rig, Brief, a **masked**
session id, last status, accumulated tokens/cost, and the update time;
a per-row **Reset** (`POST /v1/runs/runtime-state/reset`) forgets the
row — brief-scoped when the row carries a `brief_key`, behind a typed
`RESET` confirmation for the dangerous whole-Operative reset. What it
does **not** do:

- **The session id is a masked summary, never shown in full.** The
  list/table truncate it; it is a recovery pointer, not a credential
  to copy.
- **Subscription-CLI session resume is now replayed for the Codex Rig,
  same-scope only — Claude stays stored-not-replayed.** When a run starts,
  Relix looks up the stored `session_id` for the EXACT
  `(tenant, Operative, Rig, Brief)` pairing and, **for the Codex Rig**,
  threads it into the next spawn as `codex exec resume [OPTIONS] <session> -` (discrete
  argv, the trailing stdin `-` marker preserved) so a Codex Operative
  continues its prior thread instead of starting cold. This applies on every
  start path (manual `brief.run`, Prime Start-to-Shift, the autonomous
  heartbeat, and the guarded operator retry — a retry of the same line of work
  continues the same thread). The lookup is keyed on that 4-tuple, so a session
  stored under a different tenant / Operative / Rig / unrelated Brief can never
  cross in; an unknown or invalid id is skipped and the run starts fresh. What
  it still does **not** do:
  - **Claude resume is intentionally NOT mapped.** Claude Code's
    `--print --resume <session>` resolves the session from the run's working
    directory, and Relix runs every Shift in a FRESH per-run scoped workspace,
    so a resumed Claude session would not reliably resolve. Until a stable
    per-line-of-work workspace exists for Claude, only the model is applied for
    the Claude Rig (no resume). Codex threads live in `$CODEX_HOME` independent
    of the cwd, which is why Codex resume is safe.
  - **It is adapter-thread continuation, not SOL durable replay.** The SOL VM
    is still synchronous and there is no durable flow replay (see "No durable
    replay / no flow snapshots" below). Resume hands the *agent* its prior
    conversation thread; it does not snapshot/replay Relix's own flow. Reset is
    still "forget the wedged pointer", not "resume from a snapshot".
  - **The session id is never logged or surfaced beyond the existing
    masked runtime-state recovery table.** It is treated as adapter state, not
    user input, and is validated (no whitespace/control, no leading `-`) before
    it can become a discrete argv element.
- **The per-SESSION reset still has no diagnosis of its own.** Reset
  deletes the `agent_runtime_state` row; it does **not** classify the
  session and does not replay it (see "Session resume is
  stored-not-replayed" above). The **run-level** diagnosis layer below
  is a *separate*, now-shipped surface attached to the durable
  `brief_runs` Shift ledger — not to this session row. Wiring a session
  reset to that run diagnosis is future work.
  - **Durable Brief/Shift run diagnosis (v1) — SHIPPED.** Every terminal
    or refused `brief_runs` Shift is now stamped with a pure, derived
    **recovery diagnosis** (`failure_class` / `retryable` /
    `retry_budget_remaining` / `recovery_action` / `recovery_route`): a
    stable failure-class bucket (`precondition` / `governance` / `budget`
    / `adapter_unavailable` / `workspace` / `timeout` / `cancelled` /
    `interrupted` / `transient` / `permanent` / `unknown`), a
    retryable-vs-not verdict (a timeout/transient Rig failure → retryable;
    a governance / permanent / auth / config / tool-permission failure,
    and every refusal → not retryable), a **small operator-facing** retry
    budget (0 or 1, **not** an auto-retry counter), and a recommended
    action + dashboard route. It surfaces on `RunRecord` / `brief.runs` /
    the Brief detail's `latest_run` and drives the Action Center
    `failed_or_refused` recovery card + the Runs-page recovery strip.
  - **Guarded operator Shift retry (Stage-2, v1) — SHIPPED.** A
    retryable failed/interrupted Shift now has a **one-click operator
    retry**: `POST /v1/runs/:run_id/retry` (capability `run.retry`)
    opens **exactly one** child Shift through the SAME governed
    preflight/execute path (same assignee/adapter checks, Claim,
    workspace prep, run ledger, events) and links it to the source via
    durable lineage (`brief_runs.retried_from_run_id` / `retry_attempt`,
    with a partial UNIQUE index enforcing at-most-one child per source).
    The runtime **refuses** unless the source is terminal-and-failure-like,
    `retryable`, has budget, links a still-present in-tenant Brief, and has
    no existing retry child; a Claim conflict surfaces as `already_running`
    (409) and a second retry returns the EXISTING child (`already_retried`,
    no second run). The Runs page shows a **Retry Shift** button only when
    eligible. The task-level `task.retry` recovery (a separate layer on the
    Task ledger) is unchanged.
  - **Opt-in autonomous retry lane (Stage-2a, v1) — SHIPPED, default OFF.**
    A bounded **autonomous** recovery loop now exists behind an explicit
    switch (`RELIX_AUTONOMOUS_RECOVERY`, **off by default**, pace via
    `RELIX_AUTONOMOUS_RECOVERY_INTERVAL_SECS`, bound per tick via
    `RELIX_AUTONOMOUS_RECOVERY_MAX`, default `1`, clamp `1..=10`). When on,
    a timer selects retryable failed/interrupted Shifts (a tenant-aware SQL
    pre-filter whose eligibility **mirrors `retry_precheck` exactly**) and
    re-opens **exactly one** child each through the **same guarded
    `open_retry_child` path** the operator one-click uses — same tenant
    gates, duplicate-child prevention, model prefs, Codex session resume,
    workspace prep, ledger/transcript, and budget/refusal semantics. It is
    **not** a second retry path. It is **conservative**: it retries **only**
    runs already diagnosed `retryable` with budget remaining, and **never**
    a refusal, a budget hard-stop, a missing assignee/adapter, a
    permission/auth failure, a manual reject, a discarded run, or an
    exhausted-budget run. Each tick is **bounded** and **idempotent** (the
    duplicate-child guard means a re-tick opens no second child), retries
    each candidate **under its own Guild** (no cross-tenant leak), respects
    the **same per-Operative + Guild budget hard-stop** the autonomous
    dispatch enforces (over budget ⇒ skipped, quietly, no spam), and only
    runs for an **active** Operative whose **timer wake** is on. It
    chronicles a distinct `brief.autonomous_retry` event when it opens a
    child. What this is still **NOT**: there is **no LLM diagnostic pass**
    (a run lacking durable diagnosis or with an ambiguous verdict is simply
    not retried) and **no provider quota polling**; the diagnosis is still
    the pure Stage-1 classifier, and the autonomous lane never reclassifies
    or re-diagnoses. The retry **child** is committed through the shared
    preflight, so it is stamped with the shared run trigger exactly like the
    operator retry child — the autonomous provenance lives in the Chronicle
    event, not a distinct run-trigger value.

### Run-workspace review/apply is inspect-and-copy, not a full VCS workflow

A Brief Shift (run) executes in a scoped sandbox workspace and its
changed files can be **inspected** (changed-file list, secret-redacted
text preview, bounded unified diff), **reviewed** (accept/reject),
**applied** back into the configured project root, or **discarded**. A clean
apply is the operator's **review-to-done** (company-model §12.5B/§12.6): it
advances the run's Brief from `in_review` to `done`, so dependents unblock
without a separate manual `brief.move done`. **Review acceptance and apply are a
human's by default**, but can now be done **autonomously by Prime under two
SEPARATE, default-OFF standing grants** (`prime.run.review_accept` /
`prime.run.apply`, below in "Prime is autonomous over approved work") — each
acting only for a run in the candidate Mandate/proposal's own Brief set, through
these exact review/apply paths and safety checks, never a hand-rolled copy. What
it does **not** do:

- **Diff needs an intact baseline.** The unified diff
  (`/v1/runs/:id/artifacts/:aid/diff`) reconstructs the "before" side from
  the live project-root file and only when it still hashes to the run's
  recorded baseline. If the project file changed since the run, or the run
  used the `empty` workspace context (no project copy), the diff is
  honestly reported unavailable and you fall back to the file preview.
- **Preview/diff are byte-capped** (64 KiB): a very large changed file is
  truncated, not paged.
- **Apply is whole-run, all-or-nothing.** There is no per-file or partial
  apply and no `force`; if *any* file is unsafe or conflicted the whole
  apply is refused. Conflict resolution is operator-driven (re-run, or fix
  the project and retry) — there is no three-way merge.
- **Discard does not free disk immediately.** It marks the run discarded
  (and non-applyable) and leaves the sandbox for the storage prune /
  scheduled autoprune to reclaim; it never deletes a `running`
  workspace. Disk is reclaimed by `maintenance.prune` (dry-run first), not
  synchronously.
- **`git_worktree` / `git_checkout` workspace context is deferred** — only
  `empty` and the capped/filtered `copy_repo` snapshot ship today.

### Issue documents (Dossiers) are append-only textareas, not a rich editor

A Brief's documents (Dossiers — `plan` / `design` / `notes` / …) can now be
**authored and revised** from the workroom (`brief.dossier_author` →
`POST /v1/spine/briefs/:id/dossiers/author`, `relix-execution-and-issue-design`
§1.8). Authoring is **append-only and optimistic-locked**: editing the latest
revision sends its id as `expected_latest_doc_id`, so a save after a newer
revision landed is refused (**HTTP 409**, nothing written) — the draft is kept
for a reload or an explicit **fork** (which carries `forked_from_doc_id` and is
never an accidental stale overwrite). A `revision_number` is derived per
Brief+kind on read. What it does **not** do:

- **No rich text / no markdown renderer.** The editor is a plain
  kind/title/body **textarea**; the body is stored and shown verbatim. There is
  no formatting toolbar, no markdown preview overhaul, no attachments.
- **No collaborative editing.** There is no live cursor, presence, or operational
  transform — two operators editing the same kind race on the optimistic lock
  (the loser gets a 409 and reloads or forks). Single-operator-at-a-time by
  design. **Explicit document locking now exists** (below) on top of the
  optimistic lock, so a deliberate "I'm editing this, hold off" lease is
  available — but it is still single-writer, not true co-editing.
- **No external document store.** Dossiers are rows in the coordinator's
  `task_documents` ledger (append-only); there is no Google-Docs/Notion-style
  external store, no per-doc binary blobs, and the body is byte-capped (64 KiB).
- **Bounded autonomous authoring — Prime persists its own generated orchestration
  text through the governed Dossier path (v1).** The operator-facing workroom
  author/revise/fork remains a human action through the governed capability, and
  there is still **no arbitrary/freeform agent document editing**: no model-chosen
  `create_document`, no model authoring or revising an *operator's* Dossier, and no
  raw JSON doc write. What changed: the **company orchestration path**
  (`mandate.orchestrate`, the parent / role-track / subject-execution and
  placeholder plan Dossiers) now persists Prime's own deterministic-or-generated
  plan text through the **governed, append-only, lock-aware** `author_dossier` path
  (a single `TaskStore::author_prime_dossier` helper) instead of the legacy
  author-less `add_dossier`. Those writes are stamped with the synthetic
  autonomous-Prime authority `__relix_autonomous_prime__`, and the helper is
  **idempotent** (a rerun never appends a duplicate revision — `already_present`),
  **lock-aware** (a kind locked by a different subject is refused, never
  overwritten — `locked_by_other`), and **hand-edit-preserving** (a human/editor or
  legacy author-less latest revision is never clobbered — `skipped_human_owned`);
  only the first, Prime-owned `create` revision of a stable kind
  (`orchestration` / `execution` / `blocker`) is ever written, and the per-doc
  outcome is reported on the orchestration result's `dossier_notes`. The kinds are
  the flow's existing names — none are renamed. **Prime can now also author a
  *plan package* autonomously (Prime Plan-Package Authoring v1, opt-in, default
  OFF — see below), closing the old "the plan-package composer is manual-only /
  there is no autonomous LLM planner" gap.** The interactive workroom
  author/revise/fork remains a human action, and there is still **no
  arbitrary/freeform agent document editing**: no model-chosen `create_document`
  on an arbitrary kind, no model authoring or revising an *operator's* Dossier, no
  raw JSON doc write, and Prime's autonomous plan-package authoring is the bounded,
  governed path described below — not freeform editing. So Prime may author its own
  bounded, generated plan Dossiers and plan packages under governance, but it still
  does not freely edit arbitrary documents.
  - **Prime Plan-Package Authoring — Prime can autonomously OPEN a *proposed*
    Brief decomposition; it never approves it (opt-in, default OFF).** Behind
    `RELIX_PRIME_LLM_PLAN_PACKAGE` (`1|true|yes|on`, off by default) the
    autonomous / manual-tick Prime loop may open a governed **plan package** on a
    single un-decomposed Brief through the EXISTING `TaskStore::open_plan_package`
    primitive — an immutable `plan` Dossier revision + a linked `suggest_tasks`
    proposal + an approval-bound `confirm`, the interactions stamped with the
    synthetic `__relix_autonomous_prime__` authority — and **leave the confirm
    OPEN for a human**. **The model is NOT the permission system:** it authors
    only the plan title/body, the approval summary, and a bounded list of proposed
    child Briefs (title / priority / a backward `after` dependency), all
    re-validated + sanitized + secret-redacted server-side
    (`prime_plan_package::validate_plan_package`, capped at
    `MAX_AUTONOMOUS_CHILDREN`); it may **not** assign agents (children open
    unassigned), pick tools/methods/capabilities, mutate an existing Dossier,
    approve the confirm, or create children directly — acceptance still flows
    through the human `brief.plan_confirm_respond` path and the existing
    **exactly-once decomposition ledger**. On disabled / no-decider / unavailable /
    malformed output the content degrades to a deterministic safe decomposition
    with an honest provenance mode (`deterministic_only` / `llm_used` / `fallback`
    / `unavailable`), surfaced on the tick record as `plan_package_ai_mode` /
    `plan_package_ai_reason` plus the opened `plan_doc_id` / `suggestion_id` /
    `confirm_id` / `child_count`. It reuses the SAME `MeshAiDecider` / AI peer /
    session the other Prime LLM layers use — **no provider key enters the
    coordinator, web bridge, or dashboard**. It is **bounded** (≤
    `RELIX_AUTONOMOUS_PRIME_MAX` actions/tick), **tenant-scoped** (a tick for one
    Guild never reads/writes another Guild's Brief), and **dedup-guarded /
    non-clobbering**: it acts ONLY when the Mandate has a SINGLE non-terminal,
    childless Brief with **no** `plan` Dossier, **no** `plan` lock, and **no** open
    plan package, so a human/Prime/stale plan or an existing open package is never
    overwritten or duplicated (it reports `already exists` / `already awaits
    approval` and authors nothing).
    - **WHEN it fires is configurable (Prime Active Planner Trigger v2):**
      `RELIX_PRIME_PLAN_PACKAGE_TRIGGER` selects the trigger mode, *layered on top
      of* the master `RELIX_PRIME_LLM_PLAN_PACKAGE` opt-in (with the master switch
      OFF nothing is authored in any mode):
      - `tail` / `gap_fill` / blank (the **default**) — the v1 behaviour: author a
        plan package only at the **idle tick tail**, for a candidate the existing
        flow leaves idle (e.g. a Mandate whose lone Brief is `blocked`). Never
        pre-empts a start.
      - `before_execute` / `plan_before_execute` — the v2 **active planner**:
        *before* starting a lone eligible un-decomposed leaf Brief that would
        otherwise be started, open the proposed plan package **first** and **hold**
        the raw start, leaving the confirm OPEN for a human. The idle-tail
        gap-fill still runs as the catch-all. While a package is pending approval
        the start stays held; an already-planned / locked Brief (not a pending
        package) is left to start normally rather than stalling.
      - Any **unknown** value safely falls back to `tail`. The effective trigger is
        surfaced on the tick record as `plan_package_trigger` (`tail` /
        `before_execute`).
    - **Approving a Prime-authored package is now possible, but ONLY under an
      explicit standing grant (Prime Plan-Package Approval — Standing Authority
      v1).** Authoring a package never approves it; acceptance still requires a
      separate, Board-granted `prime.plan_package.approve` standing authority for
      the Guild (see the standing-authority section below). With that grant active,
      the autonomous/manual Prime tick will — *before* opening a duplicate package
      and *before* any raw start — ACCEPT/materialize an **OPEN plan-package confirm
      that autonomous Prime itself authored** (author `__relix_autonomous_prime__`),
      through the EXISTING `TaskStore::respond_plan_confirm` path and the
      exactly-once decomposition ledger (the SAME primitive a human approval uses —
      no hand-rolled child creation, no ledger bypass). It is **not** blanket
      self-approval: it is single-Brief + tenant-scoped, accepts a **Prime-authored
      package only** (a human/other-actor package is never auto-approved), consumes
      one bounded grant call only on a real materialization, and is idempotent (an
      already-accepted package neither duplicates children nor consumes a second
      grant). With **no** grant the confirm stays OPEN exactly as before — in
      `before_execute` the pending package keeps holding the start across ticks.
    - **Honest scope / remaining limits:** this is bounded plan-package *authoring*
      under a configurable trigger plus **grant-gated** approval of Prime-authored
      packages — NOT freeform document editing and **NOT** blanket self-approval
      (with no `prime.plan_package.approve` standing grant Prime never accepts its
      own confirm, and it never approves another actor's package). Even in
      `before_execute` the only thing preempted is the raw start of a **lone
      eligible leaf** Brief; it does **not**
      interrupt higher-priority governance gates (proposal / strategy approval, team
      plan, hire / Clearance), decompose multi-Brief / orchestrated Mandates, or
      scan every leaf Brief. The live bridge→model→coordinator round trip is **not**
      integration-tested in CI (the validator + deterministic fallback + the
      trigger / eligibility / dedup / tenant / exactly-once paths are fully
      unit/loop tested with scripted output).
    - **Autonomous assignment of Prime-decomposed children is now possible, but
      ONLY under an explicit standing grant (Prime-Decomposed Child Assignment —
      Standing Authority v1).** Materializing a Prime-authored package leaves its
      children **unassigned** (decompositions never inherit the parent's assignee
      by default), and `orchestrate_assign_ready` cannot adopt them — so before
      this slice the loop parked at the assignment gate until a human assigned. With
      the Board-granted `prime.brief.assign_decomposed` standing authority active for
      the Guild, the tick will — *before* the orchestration no-op — assign those
      unassigned children to the **parent Brief's own current assignee**, through the
      EXISTING `set_brief_field` `assignee` primitive (the same one the governed
      assignment paths use). It is a **narrow deterministic rule**, not a free
      assignment engine: it acts ONLY on the children of a parent whose plan-package
      `confirm` autonomous Prime itself authored and materialized (a human/other-actor
      decomposition is never touched); it assigns ONLY to the parent's current
      assignee and ONLY when that assignee is an **active, same-Guild Operative with a
      known Rig** (the model never picks an agent); it never scans arbitrary
      unassigned Briefs; it is tenant-scoped; it consumes one bounded grant call only
      when ≥1 child is actually assigned; and it is idempotent (once assigned the
      children leave the unassigned set, so a re-tick neither reassigns nor consumes a
      second grant). No parent assignee / inactive / unknown-Rig / cross-Guild assignee
      records an honest `blocked` with no assignment and no consume. With **no** grant
      the children stay unassigned and the loop parks honestly at the assignment gate
      exactly as before.
    - **End-to-end autonomy smoke (v1) — SHIPPED, what it proves.** A release-grade
      backend smoke (`prime_driver::tests::prime_autonomy_e2e_*`, run by
      `cargo test -p relix-runtime --lib prime_driver::tests::prime_autonomy_e2e`)
      drives the REAL `autonomous_prime_tick` repeatedly with a bounded `max`, the
      `before_execute` trigger, plan-package authoring on, and the
      `prime.plan_package.approve` + `prime.brief.assign_decomposed` standing grants,
      asserting the chain is real and governed end-to-end (not isolated helper tests):
      a tick **opens** the plan package before the raw start and **holds** it; a later
      tick **accepts/materializes** the Prime-authored package through the existing
      confirm + exactly-once decomposition ledger (children appear exactly once, the
      bounded grant is consumed exactly once); a re-tick duplicates neither the
      package, the approval, nor the children; a further tick **autonomously assigns**
      the unassigned Prime-decomposed children to the parent's own active echo
      assignee (no human assignment, the assignment grant consumed exactly once, a
      re-tick neither reassigns nor consumes again); the loop then **starts** them as
      durable Shifts on the safe `echo` Rig (heartbeat-trigger runs) and **honestly
      stops** at the next governance gate (run review — no `prime.run.review_accept`
      grant). What it still does **not** prove: a **live** provider/bridge round trip
      (the smoke uses the safe `echo` Rig and a scripted decider, never a real model or
      remote Rig); and autonomous assignment remains bounded to the narrow rule above
      — with **no** `prime.brief.assign_decomposed` grant, or a parent with no valid
      active assignee, the loop still parks honestly at the assignment gate (a no-op
      orchestration is reported `skipped` with no action consumed and no Chronicle
      event, not the prior livelock that re-ran orchestration every tick and falsely
      claimed `advanced`), and a human/other-actor decomposition is never auto-assigned.
      Autonomous Chronicle events for a Mandate land on a **stable anchor Brief**
      (`mandate_chronicle_anchor` — a top-level Brief, lowest task id) rather than the
      most-recently-updated Brief, so an action's provenance is deterministic.
- **Explicit document locking (v1) — SHIPPED, owner-or-nobody, refuse-not-redirect.**
  A logical Dossier (a Brief + `kind`, e.g. `plan`) can now be **locked** so
  concurrent authors don't race: `brief.dossier_lock` /
  `POST /v1/spine/briefs/:id/dossiers/lock` records a per-`(Brief, kind)` lock
  (`locked_by` / `locked_at` / optional bounded `reason`); while held,
  `brief.dossier_author` **refuses** a revision from anyone but the lock owner
  (an `Ok` body `{locked:true, kind, locked_by}` the bridge maps onto **HTTP
  409** — nothing written, never a silent overwrite). The owner keeps authoring
  normally; `brief.dossier_unlock` /
  `POST /v1/spine/briefs/:id/dossiers/unlock` releases it (**owner-or-nobody** —
  only the owner may unlock; a different subject is a 409; an absent lock is an
  idempotent no-op). Active locks are listable
  (`brief.dossier_locks` / `GET …/dossiers/locks`). Create/update/lock/unlock all
  Chronicle bounded events (`brief.dossier_locked` / `…_unlocked`), tenant-scoped
  on the owning Brief (a cross-Guild caller gets the same not-found shape as a
  missing Brief — no existence leak). What it still does **not** do: Paperclip's
  **"locked-flag → writes auto-redirect to a new key"** nicety is **not**
  implemented — a locked-document write from a non-owner is **refused** (the
  caller reloads, waits for unlock, or explicitly `fork`s), not silently
  redirected. There is also **no operator force-unlock** in this v1 (only the
  owner releases its lock), and there is no lock lease/expiry (a lock is held
  until explicitly released).

### Thread interactions: cancel + idempotent create + continuation wake (v1)

Brief thread interactions (answerable ask/confirm/suggest_tasks cards,
`relix-execution-and-issue-design` §1.9) gained three v1 additions on top of the
existing open/respond/expire lifecycle:

- **Cancel — SHIPPED.** An operator can close an open card without answering it:
  `brief.interaction_cancel` / `POST /v1/spine/briefs/:id/interactions/:iid/cancel`
  flips an `open` card to `cancelled` (Chronicles `brief.interaction_cancelled`).
  It is **idempotent** on an already-cancelled card and **refuses** a decided one
  (`resolved` / `rejected` / `expired`) — a decided card is never reopened.
- **Idempotent create — SHIPPED.** `brief.interaction_create` (JSON;
  `POST …/interactions` with an `idempotency_key`) de-duplicates on
  `(brief, author, idempotency_key)` — a repeated create returns the **existing**
  card instead of a duplicate (durable partial-UNIQUE backstop +
  check-then-insert under the store lock). A keyless create (the legacy pipe
  `brief.interaction_open` path) is unchanged and never de-duplicated.
- **Continuation wake — SHIPPED (best-effort).** Answering OR cancelling a card
  now nudges the Brief's assignee to continue, via the existing supervisory-wake
  primitive (the §1.9 "wake the assignee when answered" policy). A Brief with no
  assignee records an honest `brief.wakeup_skipped` note instead of inventing a
  wake. What it does **not** do: **supersede-on-comment** (a pending plain
  confirm auto-expiring when you just comment) is **NOT** implemented for plain
  ask/confirm cards — only the **approval-bound plan confirm** expires, and only
  when the bound `plan` revision changes under it (the existing
  `brief.interaction_expired` path). There is no per-interaction `expires_at`
  TTL/clock-driven expiry, and `suggest_tasks` materialization (accept → child
  Briefs) is the pre-existing exactly-once decomposition path, unchanged here.

### Prime is autonomous over approved work, not self-approving

Relix models a company — Founder, Prime (planning lead), Crew, Mandates,
Clearances — and the dashboard drives the whole loop (found the company →
hire a Prime → describe a goal so **Prime proposes a plan** → approve it to
create the Mandate + Briefs + crew assignments + pending hires → greenlight
spawn Clearances → **Start the work**). The `company.status` summary surfaces
the Founder, the Prime, and the crew breakdown (active / pending / by role).
The **Prime Assistant** (`POST /v1/spine/prime/propose` → `…/approve` →
`…/start`, the Chat page) turns a free-text request into a structured,
governed plan that creates nothing until approved, then starts the ready work
when you click Start or when the opt-in autonomous Prime driver is enabled and
the approved work is already ready. What it does **not** do:

- **The Prime is rule-based by default; a model can draft the plan opt-in,
  but the coordinator never calls a model itself.** The default plan is
  deterministic and request-aware — intent shapes the breakdown (a `fix` is a
  reproduce → fix → verify chain, `research` is investigate → synthesize,
  `build` is role tracks + integrate), and each Brief title carries the
  extracted deliverable, so two different requests no longer collapse to one
  shape (company-model §12.5A). **Model-assisted planning is now available but
  opt-in** (the Chat "Use AI" toggle → `mode:"ai"`): the *bridge* drafts a plan
  with the `ai` peer, then the *coordinator* validates + sanitizes +
  secret-redacts it server-side (`prime_plan::validate_model_plan`) before it is
  ever stored, and computes crew/hires/governance from the live roster — a model
  can shape only the interpretation. The coordinator handler still **never calls
  an LLM synchronously** (the AI node is a separate mesh peer); the response is
  honest about provenance via `ai_mode` (`deterministic_only` / `llm_used` /
  `fallback` / `unavailable`) + `ai_used` + `ai_status`, and **any** model
  failure (unreachable / oversized / malformed / invalid) degrades to the
  deterministic plan with an honest reason — never faked model output. What is
  **not** done: the live bridge→model→coordinator round trip is not
  integration-tested in CI (it needs a real provider; the validator + fallback
  that bound it are fully tested with fake output), and there is no
  conversational refinement — one message → one proposal.
- **A human is in the loop at every approval gate.** Prime PROPOSES; the operator
  must click **Approve & create** to create anything and greenlight each spawn
  Clearance. `prime.start`
  (company-model §12.5B) closes the loop — it turns the approved Mandate's
  ready Briefs into real Shifts through the same governed run path as a manual
  run (approved-only, ready-only, every skipped Brief reported with a reason) —
  Manual Start stays sovereign; the opt-in autonomous Prime driver can call the
  same start path for already-approved, ready proposal work while adding the
  autonomous budget hard-stop.
- **There is a bounded "guided driver" (v1) AND an opt-in bounded *autonomous*
  Prime driver (v1) — NOT a self-approving strategist.**
  `prime.next_step` (READ-ONLY) classifies the ONE next governed step for a
  proposal or Mandate over live state — approval, strategy gate, team plan + live
  readiness (hires / Clearances), the Brief board, and the run ledger
  (company-model §5.4/§8.2, the Action Center's "next step" focused onto a single
  work session). `prime.advance` then runs **at most one** safe, explicitly-
  requested step: `create_team_plan` (record a Team Plan from the Mandate's
  existing active crew — adopts active Operatives, mints **no** hires) or
  `orchestrate_assign_ready` (the existing `mandate.orchestrate` in `assign_ready`
  mode). It re-reads state and **refuses (no side effects, HTTP 409) when the
  requested action is no longer the current next step**, executes through the same
  governed handler + Keys as the manual route, and surfaces every governance
  refusal honestly.
  - **The autonomous Prime driver (v1) closes the old manual-only Prime
    caveat — opt-in, default OFF, bounded.** Behind an explicit switch
    (`RELIX_AUTONOMOUS_PRIME`, **off by default**, paced via
    `RELIX_AUTONOMOUS_PRIME_INTERVAL_SECS` default 30s, bounded per tick by
    `RELIX_AUTONOMOUS_PRIME_MAX`, default `1`, clamp `1..=10`) a timer now drives
    already-**approved** Prime/company work forward **without the operator
    clicking "Advance one step" over and over**. Each tick discovers active
    candidates (approved Prime proposals first — they carry Start — then live
    Mandates), re-classifies each with the SAME `prime.next_step` logic, and
    applies at most `max` actions: it auto-advances the safe steps
    (`create_team_plan` / `orchestrate_assign_ready`) through the **same governed
    `prime.advance` path** the operator click uses, and for an already-approved
    proposal that reaches `ready_to_start` it starts the ready Briefs through the
    **existing `prime.start` path** (no new runner). By default it
    **does not auto-approve a proposal / strategy / hire / spawn Clearance / budget gate**
    (a pending gate is recorded `blocked` and left untouched); specific proposal,
    strategy, hire, and spawn-Clearance approvals act only when the Board has
    granted the matching standing authority below, and budget is never delegated.
    it **bypasses no execution guard** — `prime.start` already enforces
    approved-only / ready-only / active-assignee / adapter-resolvable / Claim, and
    the autonomous start additionally **re-imposes the autonomous budget
    hard-stop** (`heartbeat::dispatch_budget_admits` — per-Operative Allowance +
    additive Guild budget) that the sovereign manual `prime.start` deliberately
    skips, so the loop never auto-starts an over-budget Brief (a manual Start
    stays sovereign). It is **idempotent** (each tick re-classifies live state, so
    team plans / orchestration trees / started Shifts are never duplicated and an
    already-claimed/running Brief is never double-started), **tenant-safe** (a tick
    spans all Guilds with each candidate processed under its **own** Guild, or one
    Guild when scoped), and **bounded** (≤ `max` actions/tick). It chronicles a
    distinct `prime.autonomous_advance` / `prime.autonomous_start` /
    `prime.autonomous_mandate_start` event on the Mandate's parent Brief for an
    actual action only (never per skipped gate). A **bare Mandate** (one reached
    `ready_to_start` with **no** owning Prime proposal) now has its ready
    same-tenant Briefs **started by the loop itself** through the **same shared
    guarded run pipeline** the heartbeat dispatcher and `prime.start` use
    (`heartbeat::preflight_and_spawn_with_trigger` → `preflight_run_with_prefs_trigger`
    → `prepare_claimed_run` → `execute_ready`): claims, the duplicate-run guard,
    the live adapter probe, scoped workspace prep, the durable `brief_runs` ledger,
    bridge-token minting, board advancement, and Chronicle. No second run system
    is invented — the run is just stamped as an **autonomous/heartbeat** trigger
    (not dashboard `manual`), the ready set is read tenant-scoped via
    `TaskStore::list_ready_briefs_for_tenant` and filtered to the Mandate (no
    cross-Guild Brief is selected), and the **same autonomous budget hard-stop**
    (`heartbeat::dispatch_budget_admits` against each ready Brief) blocks the whole
    start with zero runs if any ready Brief is over budget.
    **The loop is now controllable from the product at runtime — no restart, no
    env edit (Prime Runtime Autonomy Switch v1).** A **dormant watcher** is
    spawned whenever the coordinator's `SpineStore` exists, and each tick decides
    what to drive from a **tenant-scoped persisted runtime setting** + the env
    override: env `RELIX_AUTONOMOUS_PRIME` ON ⇒ drive **all** Guilds (the legacy
    behaviour, kept as a **global boot override**); env off ⇒ drive only the
    Guild(s) whose **persisted runtime toggle** is on (a runtime-off Guild is
    never driven); neither ⇒ dormant (one cheap SQL read, then sleep). The
    setting lives in the coordinator DB (`runtime_settings(tenant_id, key)`),
    flipped by the **role-gated** `prime.autonomy_set` (operator/admin only;
    `PUT /v1/spine/prime/autonomy {enabled}`) and read by `prime.autonomy_state`
    (`GET /v1/spine/prime/autonomy` → `runtime_enabled` / `env_enabled` /
    `effective_enabled` / `source` ∈ {`env`,`runtime`,`off`} + the
    max/interval/hire-Rig knobs). **Turning the loop ON is NOT an approval
    bypass** — it only wakes the driver over already-approved work; each governed
    approval still requires its own live standing grant (above). When the env
    override is set the runtime OFF control can only clear the persisted row;
    effective stays ON for every Guild until the env is changed + the coordinator
    restarts (the dashboard says so). The Settings page now exposes a live
    **Turn ON / Turn OFF** control (effective state + source + env-override
    caveat), beside the still-read-only heartbeat + recovery surfaces
    (`/v1/spine/run-config`: `autonomous_prime_enabled` / `autonomous_prime_max` /
    `autonomous_prime_interval_secs` remain the env-derived knobs).
    **The loop is also operator-wakeable on demand (Manual Autonomy Tick v1).**
    Beside the background timer, an operator can run **exactly one** bounded
    autonomous Prime tick for their Guild from the product — `prime.autonomy_tick_now`
    (`POST /v1/spine/prime/autonomy/tick`), surfaced as a **Run Prime now** button
    on Settings — and get back the per-candidate tick records
    (`{tenant, max, records:[{target_kind, target_id, mandate_id, phase, action,
    outcome, reason}], advanced, started, considered}`), so autonomous Prime is
    legible instead of mysterious. The **background runtime toggle controls the
    timer; the tick-now button is an explicit operator wake-up for one Guild and
    does NOT require the timer switch to be ON.** It is the SAME governed path the
    timer uses: **operator/admin-only** (a worker subject is denied, no side
    effect), **tenant-scoped** (it drives only the caller's own Guild, never all
    Guilds, even if the env override is set), and it still obeys every gate —
    standing approvals (no grant ⇒ every approval still left to the human), the
    autonomous-start budget hard-stop, Rig readiness, and the per-tick
    `RELIX_AUTONOMOUS_PRIME_MAX` bound. It grants **no new authority** — it only
    wakes the existing driver once.
  - **Prime standing authority (v1) — opt-in, default OFF, *grant-gated*.** Prime
    now has **two** autonomy layers. Layer (a) above is the **approved-work
    driver** (runtime toggle or `RELIX_AUTONOMOUS_PRIME` env override): it only moves work that a human already
    approved. Layer (b) is the **standing-authority driver**: when — and ONLY
    when — the Board has granted a bounded **standing approval** in the Guild, the
    same loop may also take the specific *approval* action the grant covers. This
    is **not a loop-toggle bypass**: turning the loop on wakes the driver, but
    each of the seven approval categories acts **only** while a
    `standing_approvals` row exists for the synthetic authority subject
    `__relix_autonomous_prime__` in that tenant. The categories are:
    `prime.proposal.approve` (autonomously approve a **proposed** Prime proposal
    through the existing `prime.approve` path — the proposed set is
    status-filtered + tenant-stamped, so a rejected / already-approved /
    cross-Guild proposal is never approved, and an approved proposal leaves the
    set so a re-tick never re-approves or double-consumes the grant),
    `prime.hire.approve` (activate a **pending hire created by Prime/company
    planning** — i.e. one surfaced by the Mandate's own Team Plan — bound to the
    configured safe Rig `RELIX_AUTONOMOUS_PRIME_HIRE_RIG`, default `echo`,
    validated against the known-Rig allowlist; an unknown Rig is **skipped**, not
    silently bound; an existing Rig is never clobbered), and
    `prime.clearance.approve` (greenlight a **pending spawn Clearance tied to the
    Mandate's Team Plan**, reusing the store decide path's exact side effects —
    `decide_approval` + the hire-activation hop — and refusing any non-spawn /
    tool / budget / high-risk approval), and `prime.strategy.approve` (approve a
    **proposed** Mandate strategy through the existing `mandate.strategy.approve`
    handler — the store only flips `proposed` → `approved` (`WHERE
    status='proposed'`), so a **rejected or missing** strategy is never approved
    and never re-proposed: a human **strategy rejection stays final**, and once a
    strategy is approved the next step is no longer the approval gate so a re-tick
    neither re-approves nor double-consumes), and the two **Shift-disposition**
    categories `prime.run.review_accept` (autonomously **accept** a completed
    Shift's review — a `done` + `pending_review` run that belongs to the
    candidate Mandate/proposal's own Brief set — through the existing review path
    `TaskStore::set_run_review`; only `done`/`pending_review` runs are ever
    accepted, never a rejected/discarded/accepted/applied run, and acceptance does
    **not** apply) and `prime.run.apply` (autonomously **apply** an already-
    `accepted` run through the existing safe apply machinery
    `controller_runtime::execute_run_apply` — `run_apply_eligibility`,
    baseline-hash / conflict / artifact-safety checks, and the review-to-done
    `complete_reviewed_brief` — never a hand-rolled copy; a conflicted/failed
    apply records `blocked` and **never** marks the Brief done, and is not retried
    in the same tick). **Review and apply are SEPARATE grants and SEPARATE ticks**
    — a single tick accepts XOR applies one run (the first tick accepts; the next
    applies), so neither can be combined into one broad superpower. The seventh
    category is `prime.plan_package.approve` (autonomously **accept/materialize an
    OPEN plan-package confirm that autonomous Prime itself authored** — author
    `__relix_autonomous_prime__` — through the existing
    `TaskStore::respond_plan_confirm` path and the exactly-once decomposition
    ledger, the SAME primitive a human approval uses; it is single-Brief +
    tenant-scoped, runs **before** opening a duplicate package or any raw start, and
    accepts a **Prime-authored package only** — a human/other-actor package is never
    auto-approved. With no grant the confirm stays OPEN, so a pending `before_execute`
    package keeps holding the start; once accepted the confirm is `resolved`, so a
    re-tick neither duplicates children nor consumes a second grant). The eighth
    category is `prime.brief.assign_decomposed` (autonomously **assign the unassigned
    child Briefs of a Prime-authored decomposition** to the **parent Brief's own
    current assignee**, through the existing `set_brief_field` `assignee` primitive,
    and ONLY when that assignee is an active same-Guild Operative with a known Rig —
    the model never picks an agent; it runs **before** the orchestration no-op, acts
    on Prime-decomposed children only — a human/other-actor decomposition is never
    touched — never scans arbitrary unassigned Briefs, and consumes one bounded grant
    call only when ≥1 child is actually assigned. With no grant the children stay
    unassigned and the loop parks honestly at the assignment gate; once assigned the
    children leave the unassigned set, so a re-tick neither reassigns nor consumes a
    second grant; an absent/inactive/unknown-Rig parent assignee records an honest
    `blocked` with no assignment and no consume). Each autonomous approval **consumes** one
    call of a bounded grant (`max_calls` / `max_cost_micros`); an unlimited grant
    is not decremented (existing standing-approval semantics). It is **tenant-safe**
    (a grant in Guild A never approves Guild B's proposal/hire/Clearance/strategy — the
    check is per the candidate's own Guild), **bounded** (still ≤
    `RELIX_AUTONOMOUS_PRIME_MAX` actions/tick), and **idempotent**. Grants are
    made/revoked through the EXISTING `agent.standing_approval.*` routes
    (`POST`/`DELETE /v1/agents/__relix_autonomous_prime__/standing-approvals`) —
    the same routes real Operatives use, so **no duplicate approval system was
    invented**. **The Settings page is now an operator control surface, not
    read-only:** each of the eight categories shows enabled/disabled with a
    **Grant** (when disabled) / **Revoke** (when enabled) button. Granting creates
    a bounded standing approval for the synthetic authority (default `expires_at =
    now + 24h`, `max_calls = 25`, no cost cap); revoking deletes every row for that
    category. The same bounded grant is scriptable from the CLI
    (`relix-cli agent standing-approval-grant --agent-id __relix_autonomous_prime__
    --category prime.proposal.approve --expires-in 24h`) or `curl` against the same
    routes. The live per-Guild category state is still reflected at
    `GET /v1/spine/prime/standing-authority`. **The normal approval system is not
    weakened**: granting in the dashboard does **not** enable autonomy — the loop
    only acts when the Prime loop is *also* effectively on (runtime toggle or env override), and even then a category
    acts only while its grant is live. With no standing grant, every approval gate
    stays human exactly as before — autonomy here only *adds* power when the Board
    has explicitly granted it, inside the bound it set.
  - **Prime Strategy Drafting (v1) — Prime DRAFTS a Mandate strategy; it does not
    APPROVE it by default (only under an explicit `prime.strategy.approve` standing
    grant, above).** Previously a Mandate with no strategy was a dead
    stop until an operator hand-wrote and proposed a strategy doc. Now, when a
    Mandate has **no strategy yet**, the guided/autonomous driver classifies it as
    `needs_strategy_proposal` (`advance_action = "propose_strategy"`,
    `can_advance = true`) and can compose a strategy doc from the
    Mandate's own fields (title / description / status) + the Guild's active work
    roles and propose it through the **existing** `mandate.strategy.propose` path
    (`draft_mandate_strategy` → `handle_strategy_propose`). The body is
    **deterministic by default** and **opt-in model-authored** under
    `RELIX_PRIME_LLM_STRATEGY_DRAFT` for the autonomous/manual-tick loop (Prime
    Strategy Authoring v1, below). The manual
    one-click `prime.advance {action:"propose_strategy"}` and the opt-in autonomous
    Prime tick both run it through that same governed handler, stale-guarded exactly
    like the other advance actions (the explicit one-click route stays
    deterministic). **It is emphatically NOT strategy approval:** the
    doc lands `proposed` (it surfaces in the Action Center / Approvals as an
    `approval` item, and the next governed step becomes the human
    `mandate.strategy.approve` gate); team planning and orchestration stay locked
    until that gate is approved. **By default that approval is a human's.** It
    becomes autonomous **only** when the autonomous Prime loop is effectively ON
    (runtime toggle or env override) **and** the Board has granted the
    `prime.strategy.approve` standing authority for the Guild (above) — then the
    same loop approves the *proposed* strategy through the existing
    `mandate.strategy.approve` handler, consuming one bounded grant call. **A
    strategy rejection stays final** — the store only flips `proposed` →
    `approved`, so a rejected (or missing) strategy is never auto-approved or
    re-proposed; only a human proposing a fresh strategy reopens the gate.
    It is **idempotent and non-destructive** — a strategy already `proposed`,
    `approved`, or `rejected` is **never overwritten** (a re-advance refuses as
    `stale_action`), so a human **rejection** is honoured: Prime does not turn
    around and re-propose to fight the operator. It is **tenant-scoped** (a tick for
    Guild A never drafts Guild B's strategy), **bounded** (consumes one autonomous
    tick action when it proposes), and **chronicled only when it actually proposes**
    — a distinct `prime.autonomous_strategy_proposed` event is appended to the
    Mandate's parent Brief if one exists (a strategy is usually drafted *before*
    orchestration, so there is often no Brief yet and the `PrimeAutonomyRecord` is
    the only trace, by design — an idle/skipped tick spams nothing). The draft body
    is **deterministic by default** — a structured objective / constraints /
    team-tracks / execution / review-apply / risk-and-approval doc derived from the
    Mandate, sanitized for the pipe-delimited wire and length-bounded — and is
    **opt-in model-authored** when `RELIX_PRIME_LLM_STRATEGY_DRAFT` is on (see Prime
    Strategy Authoring below). Either way the doc is only ever **proposed**, never
    approved by the model. **No new provider/key system was added.**
  - **Prime Strategy Authoring (v1) — the proposed strategy *body* can be
    model-authored; the strategy is still only PROPOSED, never approved by the
    model (default OFF).** Behind `RELIX_PRIME_LLM_STRATEGY_DRAFT` (`1|true|yes|on`,
    off by default), when the autonomous/manual-tick Prime loop executes
    `propose_strategy` and a live mesh AI decider is available, a model authors the
    strategy *text* from the SAME bounded, secret-free snapshot the deterministic
    draft uses (Mandate title / status / bounded description / active work roles /
    Brief readiness counts) — never secrets, credentials, tokens, repo/file content,
    or huge dumps; the prompt is length-capped. **The model is not the permission
    system:** its reply is fully re-validated + sanitized server-side by
    `prime_strategy::validate_strategy_draft` (rejects empty / overlong /
    prompt-injection boilerplate; sanitizes the pipe to `/` and non-whitespace
    control chars; appends a standard "DRAFT / not approved" governance footer when
    the model omits it; bounds the final doc to `STRATEGY_DRAFT_BODY_CAP` with the
    footer preserved), and is **only ever proposed** through the EXISTING
    `mandate.strategy.propose` handler — the human `mandate.strategy.approve` gate is
    unchanged, and an existing `proposed`/`approved`/`rejected` strategy is **never
    overwritten** (the classifier only yields `propose_strategy` for a Mandate with
    no strategy, so a human rejection stays final). On unavailable / malformed /
    unsafe / disabled output the body degrades to the deterministic
    `draft_mandate_strategy` with an honest provenance mode
    (`deterministic_only` / `llm_used` / `fallback` / `unavailable`), surfaced on the
    tick record as `strategy_ai_mode` / `strategy_ai_reason` (distinct from the
    action-choice `ai_mode`). It reuses the existing governed `ai.chat` mesh path
    (the SAME `MeshAiDecider` + AI peer / session the deliberation layer uses) — **no
    provider key enters the coordinator, web bridge, or dashboard**. **Honest scope:**
    model-backed strategy authoring is wired into the **autonomous loop and the
    manual `Run Prime now` tick** only; the explicit one-click
    `prime.advance {action:"propose_strategy"}` route stays **deterministic** by
    design (it never builds a decider).
  - **Prime Deliberation (v1) — the autonomous loop is no longer a hardcoded
    deterministic state machine; an opt-in model may CHOOSE among the already-computed
    governed actions (default OFF).** Behind `RELIX_PRIME_LLM_DELIBERATION`
    (`1|true|yes|on`, off by default) each autonomous tick still computes the SINGLE
    legal next governed action for a candidate exactly as before, then asks an opt-in
    model — as an advisory pre-pass — to either CONFIRM that action or HOLD (`none`)
    this tick. **The model is NOT the permission system.** Its choice is constrained
    to `[<computed action>, none]` by a strict server-side validator
    (`prime_deliberation::parse_prime_decision`): an unknown action, an action outside
    the candidate's allowed set, malformed/array/scalar/over-long JSON, an over-long or
    control-char reason, or model prose all degrade to the deterministic behaviour with
    an honest mode. A `none` skips the candidate this tick with **zero side effects**;
    a confirm runs the EXACT SAME governed handler + standing authority + budget gate +
    Claim + adapter probe + tenant isolation as before — the model can never invent an
    action, widen the legal set, approve a gate it lacks a standing grant for, or
    bypass any budget/Claim/adapter check. Every tick record carries the provenance
    (`ai_mode` ∈ {`deterministic_only`,`llm_used`,`fallback`,`unavailable`} +
    `ai_reason`), surfaced on `prime.autonomy_tick_now`. The live decider performs only
    the EXISTING `ai.chat` mesh call to the AI peer (alias `RELIX_PRIME_AI_PEER`,
    default `ai`; session `RELIX_PRIME_LLM_SESSION`, default `prime-autonomy`) using the
    same `{session_id,prompt,history}` shape as the bridge — **no provider key ever
    enters the coordinator, web bridge config, or dashboard**. A missing mesh / AI peer
    produces `unavailable` and falls back deterministically; the per-call deadline is
    clamped to 5–60s so the loop never blocks. **Honest scope:** this is *constrained
    deliberation over the existing action menu*, NOT freeform tool-calling Prime — the
    model confirms-or-holds a single computed action and attaches a reason; it does not
    author strategy, invent goals, pick identities to hire, or call tools. The
    live bridge→model→coordinator round trip is **not** integration-tested in CI (it
    needs a real provider; the validator + the deterministic fallback that bound it are
    fully unit/loop tested with scripted output). The **manual** tick
    (`prime.autonomy_tick_now`, the **Run Prime now** button) now wires the SAME live
    `MeshAiDecider` as the background timer: with deliberation ON it exercises live
    deliberation whenever the coordinator's outbound mesh client (the populated alert
    mesh cell) and the AI peer are reachable, and the controller runs the tick from a
    blocking thread so the decider's `Handle::block_on` never executes on an async
    worker. The remaining honest caveat is narrower: live AI deliberation depends on a
    **populated coordinator mesh client and a reachable AI peer** — when the mesh cell
    is unpopulated or the peer is unreachable the manual tick honestly reports
    `unavailable` and runs deterministically.
  - **Prime Executive Prioritization (v1) — candidate discovery/order is no longer
    fixed-deterministic-only; an opt-in model may CHOOSE the ORDER in which a bounded
    tick spends its action budget across the already-computed legal candidates
    (default OFF).** Behind `RELIX_PRIME_LLM_PRIORITIZATION` (`1|true|yes|on`, off by
    default) the loop first builds the SAME deterministic candidate queue as before
    (the FALLBACK order) and classifies each candidate **read-only** into the same
    next governed action it would run today, then — only when ≥2 candidates carry a
    positive **attemptable** action — asks an opt-in model to ORDER the offered
    candidate keys (or return an empty order to HOLD the whole queue this tick).
    **The model is NOT the permission system.** Its order is constrained to the
    offered keys by a strict server-side validator
    (`prime_priority::parse_priority_order`): an unknown key, a duplicate, a
    non-array/missing `order`, more keys than offered, a non-string key,
    malformed/array/scalar/over-long JSON or prose, or an over-long/control-char
    reason all degrade to the deterministic discovery order with an honest mode. The
    model can never invent a candidate, add an action to the menu, widen a
    candidate's allowed action, approve a gate it lacks a standing grant for, or
    bypass any budget/Claim/adapter/tenant scope — only the deterministic
    classifier's already-attemptable candidates are offered, and each executed step
    flows through the EXACT SAME governed handler + gates as before. An empty order
    holds the queue with **zero side effects**; with `MAX=1` the model can now elevate
    the genuinely-most-important legal candidate above the deterministic first.
    Every tick record carries the provenance (`priority_ai_mode` ∈
    {`deterministic_only`,`llm_used`,`fallback`,`unavailable`} + `priority_ai_reason`
    + this candidate's `priority_rank`), surfaced on `prime.autonomy_tick_now` and the
    Settings tick table (`ord:`). The live decider reuses the SAME `MeshAiDecider` /
    AI peer / session the deliberation + strategy layers use (built when ANY of the
    three switches is on) — **no provider key enters the coordinator, web bridge, or
    dashboard**; a missing mesh / AI peer produces `unavailable` and falls back
    deterministically. **Honest scope:** this is *queue prioritization among the
    already-computed legal candidates*, NOT freeform goal invention or arbitrary
    tool-calling — the model reorders (or holds) the attemptable menu; it does not
    author actions, invent goals, pick identities to hire, approve gates, or call
    tools. With the switch off (or <2 attemptable candidates) the discovery order is
    byte-for-byte the legacy behaviour. The live bridge→model→coordinator round trip
    is **not** integration-tested in CI (the parser + deterministic fallback that
    bound it are fully unit/loop tested with scripted output).
  - **Prime Orchestration Authoring (v1) — the orchestration Brief *text* is no
    longer mechanical-only; an opt-in model may AUTHOR the titles / dossiers /
    checklists of the already-computed Brief skeleton (default OFF).** Behind
    `RELIX_PRIME_LLM_ORCHESTRATION` (`1|true|yes|on`, off by default) the autonomous /
    manual Prime tick still materialises the EXACT SAME idempotent Brief tree
    (`mandate.orchestrate` in `assign_ready` mode — parent → role tracks → subject
    executions, with placeholder tracks for staffing gaps), but the human-facing TEXT
    of NEWLY-created parent / role-track / subject Briefs may be model-authored from a
    bounded, secret-free snapshot (Mandate title/status, a bounded approved-strategy
    excerpt, the active role keys + their staffed agent ids, gap roles + reasons,
    `max_briefs`). **The model is NOT the permission system.** Its blueprint is keyed
    STRICTLY by the offered role / subject keys and re-validated server-side
    (`prime_orchestration::parse_orchestration_blueprint`): an unknown role/subject
    key, an unknown top-level key, an array where an object is expected, an
    over-long title/dossier/checklist item, too many checklist items, malformed JSON
    or prose all degrade to the deterministic titles + dossiers with an honest mode.
    The model can **only** change Brief text — it can never invent a role, agent,
    Brief id, source marker, dependency, assignee, approval, budget change, or tool;
    the roles, agents, assignments, reviewer stamping, gates, `max_briefs` cap,
    placeholder behaviour, and source-marker idempotency are byte-for-byte identical
    to the deterministic path, an existing/hand-edited Brief title is **never**
    clobbered on rerun (reuse is by source marker; titles are set on creation only),
    and placeholder-track text stays deterministic. Every orchestrate tick record
    carries the provenance (`orchestration_ai_mode` ∈
    {`deterministic_only`,`llm_used`,`fallback`,`unavailable`} +
    `orchestration_ai_reason`), surfaced on `prime.autonomy_tick_now` and the Settings
    tick table (`orch:`). The live decider reuses the SAME `MeshAiDecider` / AI peer /
    session the other Prime LLM layers use — **no provider key enters the coordinator,
    web bridge, or dashboard**; a missing mesh / AI peer produces `unavailable` and
    falls back deterministically. **Honest scope:** this authors the *text* of an
    already-allowed skeleton only; the direct one-click `mandate.orchestrate` /
    `prime.advance {action:"orchestrate_assign_ready"}` route stays **deterministic**
    by design (it never builds a blueprint). The live bridge→model→coordinator round
    trip is **not** integration-tested in CI (the parser + deterministic fallback that
    bound it are fully unit/loop tested with scripted output).
  - What this still does **NOT** do: there is **no freeform model reasoning or
    tool-calling** — the deliberation above is constrained to confirm-or-hold the ONE
    computed governed action, the prioritization above only reorders (or holds)
    the already-computed legal candidate queue, and the orchestration authoring above
    only writes the *text* of the already-computed Brief skeleton (none can invent a
    goal, role, agent, assignment, or call a tool). A model **may**
    now author the *text* of a PROPOSED strategy (Prime Strategy Authoring, above)
    when its switch is on, but only the **body** of a `proposed` doc — it does not
    approve the strategy, choose the action, pick which person/identity to hire, or
    invent a goal from raw intent; with the switch off the body is a **deterministic**
    doc. It **drafts a strategy proposal and — by default — does not approve it,
    does not decide which person/identity to hire, and does not invent a goal from
    raw intent**. **Raw goal creation still starts from a submitted
    goal/proposal** — Prime proposes a plan from a request; it never conjures a
    goal from nothing. The standing-authority layer may *approve* a proposal,
    *approve* a proposed strategy, *activate* a planning hire, or *greenlight* a
    planning Clearance — but only **inside the bounded authority the Board explicitly
    granted** (the `standing_approvals` row), only for items **attributable to
    Prime/company planning** (a proposal in the proposed set; a *proposed* strategy
    on the Mandate; a hire/Clearance in the Mandate's
    Team Plan), and never for an arbitrary tool/budget/high-risk approval. With
    **no grant**, proposing/approving plans, greenlighting hires/Clearances, and
    the Guild-budget ceiling all remain the human/Board's, exactly as before. The
    default Prime is still rules; a model can shape only the *interpretation* of a
    propose request and the *text* of a PROPOSED strategy (never crew, governance,
    approvals, or the action choice). So the Board/human approval gates are
    preserved by default, and autonomy operates strictly **inside** them — either
    after a human approval (layer a) or within an explicit standing grant
    (layer b).
- **Hiring is request → approve only.** Prime suggests *which roles* are
  missing and files them as `pending` hire requests on approval, but it does
  not decide *which person/identity* to hire, and a pending hire is inert until
  the operator greenlights it. The operator does choose the **adapter**: the
  governed `agent.approve_hire` (`POST /v1/agents/:id/approve-hire`) accepts an
  optional `{rig}` and binds it atomically at approval (company-model §12.6), so
  a greenlit hire is *immediately runnable* in one call — for the safe-local
  loop that Rig is the built-in `echo` (validated against the known-Rig
  allowlist; a duplicate/conflicting approval never clobbers an already-bound
  Rig). Approving without a `rig` still works and the response's `needs_rig`
  flag says a Rig must be configured before the Operative can run. It never
  *silently* assigns a paid/interactive CLI — the operator names the Rig.
  **Crew is reused before it is hired.** `mandate.team_plan` first adopts an
  already-active, runnable same-role Operative in the Guild (the oldest match,
  tenant-scoped) and files a `pending` hire only for a role with no such crew —
  so a build plan staffs the existing engineer/designer instead of duplicating
  them (company-model §12.5A/§12.5B). It still does not decide *which identity*
  to hire for a genuinely missing role.
- **The first-run "starter crew" runs safe-local echo work only.** A brand-new
  company has no work-role Operatives, so `prime.start` would skip every track.
  `company.starter_crew` (`POST /v1/spine/company/starter-crew`, owner-gated,
  idempotent, company-model §12.6) closes that gap by provisioning the Founder
  plus a couple of clearly-labelled **safe-local Operatives on the built-in
  `echo` Rig** (default engineer + designer) — so the operator can run the full
  propose → approve → **start** loop and watch a real Shift finish **without
  installing or logging in to any external coding agent**. It does **not**
  provision or authenticate Claude/Codex (or any real adapter): reaching real
  provider-authenticated execution still requires installing + logging in to a
  coding-agent CLI on the Settings page and switching an Operative's Rig to it.
  The starter Operatives are plain workers (no spawn/assign Keys) and are
  created directly only as the Board's sovereign first-run action.
- **A per-Operative model preference is now CONSUMED by the supported CLI
  adapters — with two stated gaps.** An Operative's stored `model_preference`
  / `reasoning_effort` flows from its profile into the Rig run on every start
  path (manual `brief.run`, Prime's Start-to-Shift, the autonomous heartbeat)
  and onto the subscription CLIs' argv: the Claude Rig gets `--model <model>`,
  the Codex Rig gets `--model <model>` + `-c model_reasoning_effort=<effort>`
  (adapters §3.2/§3.3). The **guarded operator retry** (`run.retry`) now
  **inherits** these prefs too: the bridge resolves the retry child's assignee
  with a tenant-scoped Operative lookup and threads `model_preference` /
  `reasoning_effort` through `open_retry_child` → `preflight_run_with_prefs`, so
  a retry child runs on the SAME model the original Shift would (no silent
  downgrade to the adapter default; an Operative with no preference stays clean).
  It is argv-only (no shell), and echo / Gemini / generic Rigs ignore it.
  **Remaining:** **Claude effort is not mapped** — Claude Code exposes no
  documented headless reasoning-effort flag, so only the model is applied for the
  Claude Rig. An invalid model name is the CLI's own run-time error (Relix passes
  it as a discrete argv value, never a shell token).
- **The autonomous heartbeat only executes**, it does not plan. It runs
  already-assigned Briefs on a timer — it never authors strategy, staffs a
  team, or orchestrates a Mandate.
- **No org-graph visual** beyond a shallow reports-to list.
- **The Live Shift Room now has a dedicated session stream, with polling as a
  fallback.** After `prime.start`, the Chat approved-plan card shows a live
  Shift Room (each Brief's latest Shift, blockers, review/apply state, and a
  next-action button) sourced from the READ-ONLY `prime.status` capability. The
  dashboard **prefers a dedicated per-session SSE stream**
  (`GET /v1/spine/prime/proposals/:id/status/stream`): the server emits the
  initial status snapshot immediately, **reuses** the existing run-event feed
  (`run.events.recent`) only as a cheap change-trigger so a Shift transition
  reflects within ~1 s, and **force-refreshes on a low (~3 s) interval** so the
  room still converges if an event is missed or the run-event source is absent.
  Identical frames are de-duped (keep-alive ping only), so the loop never spins.
  When the stream **isn't** connected the dashboard falls back to **polling the
  snapshot every 4 s** and the header badge honestly reads `polling` (it only
  says `live` when the stream is actually connected) — it never claims realtime
  when the stream is unavailable. A tenant-gated / unknown proposal emits a
  terminal `event: not_found` (no existence leak) and stops cleanly. The stream
  invents **no new state or event table** — it composes the same read
  capability the polling route uses. `prime.status` itself **never starts,
  applies, or discards** anything (those remain the existing explicit routes);
  when a relation is unknowable it returns honest partial data (e.g.
  `latest_run:null`), never a fabricated state. So a finished/blocked Shift
  still appears within ~1 s of its run event (or one forced-refresh / poll
  interval) — low-latency, not hard-realtime push of every field.
- **Shift-Room blockers are tenant-scoped.** `prime.status` reads a Brief's open
  blockers (Snags) through `list_snags_for_tenant`, which filters the related
  (blocker) Brief to the proposal's own Guild. Even a **legacy `blocked_on` edge
  that crosses Guilds** can never surface a cross-tenant blocker id or title in
  the Shift Room — pinned by a coordinator test that forces such an edge.
- **The Action Center surfaces what needs the operator, across the whole
  company — but it is a read-only feed, not every signal.** `company.actions`
  (`GET /v1/spine/company/actions`, company-model §5.4/§8.2) composes EXISTING
  live state into one ordered, deduped feed on the Overview Command Center:
  pending approvals/Clearances + proposed strategies (`approval`), pending hires
  (`hire`), **budget alerts** (`budget`), ready-to-start Briefs
  (`ready_to_start`), missing-assignee + dependency-blocked Briefs (`blocked`),
  runs awaiting review (`needs_review`), failed/refused/interrupted runs
  (`failed_or_refused`, now a recovery-decision card), and stale work (`stale`).
  It is **READ-ONLY** (it approves/runs/applies nothing — each row links to the
  existing governed route), **tenant-scoped** (no cross-Guild leak), and
  **invents no notification table** — live state is the source. The Overview card
  now **refreshes** off the existing run-event SSE (debounced change-trigger)
  with a low-frequency (20 s) poll fallback; it updates only on a successful
  fetch (a transient blip never blanks it) and stays stable if the stream is
  absent — **no new event bus**.
  - **Budget alerts now carry live spend — from the SAME source the gate
    enforces.** The `budget` category has two kinds of signal, kept clearly
    distinct:
    - **Committed-Allowance planning signals** (capacity *reserved*, from
      configured Allowance state): **(a)** committed Allowance (sum of active
      Operatives' caps) over/near the Guild budget — only when the Guild has a
      positive budget set — and **(b)** an active Operative hard-stopped by a `0`
      Allowance that has runnable/blocked work assigned.
    - **Live actual-spend signals** (money already *spent*): when the metrics
      ledger is wired, the handler reads the **authoritative month-to-date spend
      the dispatch gate itself enforces** — `MetricsQuery::cost_since` summing
      `cost_micros` over the **current UTC calendar month** (the canonical
      `heartbeat::allowance_window`), the exact source + window of the
      `over_allowance` / `over_guild_budget` refusals
      (`heartbeat::allowance_admits` / `guild_allowance_admits`) — through a
      read-only `SpendSource` seam, and emits: a per-Operative **spend over/near
      its Allowance** alert (over = at/over the cap = the gate's refusal
      threshold), and a **Guild spend over/near budget** alert. Each carries the
      real `spent / limit / percent` in its reason (e.g. *"spent $250.00 of the
      $200.00 monthly Allowance (125%) this month"*).

    Honest scope and the remaining gaps:
    - **No fabricated spend.** When metrics are disabled (`[metrics] enabled =
      false`) the `SpendSource` is `None`, so **no spend item appears at all** —
      only the committed/hard-stop allowance signals. A transient ledger read
      error is treated as "no signal", never as a `0`.
    - **Committed ≠ spent.** The committed-Allowance item (capacity reserved) and
      the actual-spend item (money spent) are separate objects with separate ids
      — they never collapse onto one another and never double-count the same
      money.
    - **Window.** Spend is the **current UTC calendar month** (the canonical
      `heartbeat::allowance_window` — month start inclusive, reset at the next
      month boundary), the SAME window the dispatch gate bills against, compared
      against the Guild's *monthly* budget / Operative *monthly* Allowance and
      stated literally in every reason line so no two windows are silently mixed.
      The near band is 80% (the gate refuses at 100%).
    - **Tenant isolation.** Guild spend is the **sum of this tenant's own
      Operatives'** per-agent `cost_since`; the handler never issues a
      company-wide `cost_since(None, …)`, so no other Guild's spend can leak into
      this Guild's totals.
    - **The Guild budget is now an autonomous hard-stop, not alert-only.** The
      dispatch gate enforces TWO ceilings on the autonomous path
      (`heartbeat::dispatch_budget_admits`): the per-Operative Allowance
      hard-stop (`allowance_admits`, authoritative, takes precedence) and — when
      the per-Operative gate allows — the **additive Guild-budget hard-stop**
      (`guild_allowance_admits`). When this Guild's month-to-date spend
      (the tenant-scoped sum of its own active Operatives' `cost_since`) reaches
      its `Guild.monthly_allowance_cents` budget, the heartbeat **refuses** new
      autonomous runs of that Guild's Briefs, parks the Brief in `blocked`,
      chronicles `guild.budget_refused`, and records a durable refused run with
      status `over_guild_budget` (distinct from the per-Operative `over_allowance`
      so run history reads which ceiling refused). The budget alert above is the
      operator-facing surface of this same gate (same figure + window), not a
      separate not-enforced signal. **Manual operator runs stay sovereign** —
      `preflight_run` / `brief.run` / `prime.start` take no budget gate, so the
      Board can always run an over-budget Brief by hand. The gate is **inert
      (allows) when the SpineStore or metrics ledger is unavailable** — it never
      fabricates a Guild stop from missing spend. A Guild budget of `0`/unset
      means "no cap" (deliberately distinct from the per-Operative `0` =
      hard-stop). Real per-Operative over-spend also continues to surface
      *reactively* as the `over_allowance` recovery card.
    - **The Approvals hub renders these budget alerts read-only.** The typed
      Approvals hub (`/approvals`) groups Clearances by type and shows a per-type
      payload summary, but the `budget`-category items are **informational only** —
      there is **no inline budget-decision route**, so the hub labels each by kind
      (spend alert vs committed-Allowance plan vs hard-stop) and links out to
      **Costs / Operatives** rather than offering a fake "decide". The Clearance
      payload summary is derived from the fields the runtime actually records
      (`subject_id` / `capability_category` / `expires_at` / `task_id` + method +
      reason); there is **no free-form resource/scope/payload editor** because the
      runtime stores none, and **no new approval authority is created** — the
      runtime cap remains the sole authoriser and nothing is auto-approved.
  - **The `failed_or_refused` recovery card now carries a true
    retryable-vs-not verdict** — drawn from the durable per-run diagnosis layer
    (`failure_class` / `retryable` / `retry_budget_remaining` on `brief_runs`;
    see "Durable Brief/Shift run diagnosis" above). The card prefers the run's
    stamped `recovery_action` / `recovery_route` (falling back to the older
    refusal-reason → action/route map for legacy rows), and rides the
    failure-class + retryable + remaining-budget along so the dashboard shows an
    honest recovery-class + retryable badge. It is **conservative**: a refusal is
    never marked retryable (it needs an operator fix first), and the Action
    Center card still mints **no retry button** — it points at the EXISTING
    governed route (the one-click Shift retry lives on the **Runs page**, which
    carries the run id safely; see "Guarded operator Shift retry" above).
  - What it still does **not** do: classify the finer `blocked` sub-reasons as
    distinct reasons; run an **LLM diagnostic pass** or poll provider quotas (the
    diagnosis is still the pure Stage-1 classifier — see the opt-in autonomous
    retry lane above, which retries already-diagnosed-retryable Shifts but never
    re-diagnoses); or push every field in hard-realtime (the refresh is
    event-trigger + poll; the feed is capped at 60 with an honest `truncated`
    flag). The Action Center card itself still mints **no retry button** —
    autonomous retries happen on the timer (opt-in), and the operator one-click
    lives on the Runs page.

In short: the *governance rails* of a company are in place and tenant-safe,
the Shift Room makes the post-start loop legible (what ran / finished / is
blocked / needs review, with the next action one click away) over a dedicated
low-latency status stream (polling fallback, honest badge), a model can
now draft the plan **opt-in** behind a server-authoritative validator, a
bounded **guided driver (v1)** names the ONE next governed step and can
advance **one** safe step at a time on explicit operator click (`prime.next_step`
/ `prime.advance` — `create_team_plan` / `orchestrate_assign_ready` only, stale-
safe, governance unchanged), and an **opt-in bounded *autonomous* Prime driver
(v1)** (default OFF, `RELIX_AUTONOMOUS_PRIME`) now drives already-**approved**
work forward on a timer — planning the team, orchestrating the Brief tree, and
starting ready work through the same governed routes, bounded + idempotent +
tenant-safe, with the autonomous budget hard-stop re-imposed on auto-start. On
top of that, an **opt-in standing-authority layer** lets the SAME loop also
approve a proposal, approve a proposed strategy, activate a planning hire, or
greenlight a planning Clearance
— but **only inside a bounded standing approval the Board explicitly granted**
to the synthetic `__relix_autonomous_prime__` authority for that Guild (never
from env alone, never for an arbitrary tool/budget/high-risk approval, always
tenant-scoped + consumed, and never reviving a rejected strategy). The default
Prime is still rules, the model only
shapes the *interpretation* (never crew/governance), and with **no standing
grant** the loop **never** auto-approves a strategy / proposal / hire / spawn /
budget / Clearance gate — so by default approvals + strategy + hire decisions +
the Guild budget
ceiling stay human, raw goal creation still starts from a submitted
goal/proposal. Prime now **drafts** a Mandate strategy and proposes it (Prime
Strategy Drafting v1, above) — deterministic by default, or **opt-in
model-authored** for the *body* under `RELIX_PRIME_LLM_STRATEGY_DRAFT` (Prime
Strategy Authoring v1, above) — but either way that is a *draft*, left `proposed`
for a human to approve, never approved or executed by the model itself, and never
a driver that takes a goal from raw intent to done autonomously. With **Prime
Deliberation v1**
(opt-in, `RELIX_PRIME_LLM_DELIBERATION`, off by default) the autonomous loop is no
longer a hardcoded deterministic state machine: a model may **choose among the
already-computed governed actions** (confirm the one legal next action or hold), but
the model is **not** the permission system — a strict server-side validator bounds
its choice to `[<computed action>, none]`, every confirmed action still flows through
the same governed handlers + standing authority + budget + Claim + adapter + tenant
gates, and any malformed/disallowed/unavailable output falls back deterministically
with an honest `ai_mode`. With **Prime Executive Prioritization v1** (opt-in,
`RELIX_PRIME_LLM_PRIORITIZATION`, off by default) candidate discovery/order is no
longer fixed-deterministic-only: a model may choose only the **order** in which a
bounded tick spends its action budget across the already-computed legal candidates
(or hold the queue), validated to the offered candidate keys only — it cannot invent
a candidate, add or widen an action, or bypass any gate, and invalid/unavailable
output falls back to the deterministic discovery order with an honest
`priority_ai_mode`. No provider key enters the coordinator / web bridge /
dashboard (the live path only makes the existing `ai.chat` mesh call). Autonomy
operates strictly **inside** the Board's gates — after a human approval, or within an
explicit standing grant.

### Bridge persists every chat as a Task (fail-soft)

(Formerly a documented gap; now **closed** — see git history.)
All three chat-bearing endpoints (`/chat`, `/chat_with_tool`,
`/v1/chat/completions`) auto-create a Task on the Coordinator before
the flow runs, append `task.created` + `flow.started` (and
`capability.invoked` for the tool path), and write a terminal
`task.update(status=completed|failed)` + `task.completed` /
`task.failed` event when the flow finishes.

The integration is **fail-soft**: every `task.*` call from the bridge
warns-and-skips on Coordinator failure. Chat requests never block on
Coordinator availability. The `task_id` is omitted from the response
JSON entirely when persistence was not wired or failed, so strict
OpenAI clients never see a field they don't expect.

What's still **not** done:
- Per-`remote_call` events. The bridge writes flow-level events
  (`task.created`, `flow.started`, `task.completed`/`task.failed`)
  plus a single `capability.invoked` on the tool path. Per-call
  detail lives in the existing per-flow event log on disk, which
  `task.latest_flow_log_path` points at.
- Status transitions through `running`. The current path writes
  `pending` → `completed` (or `failed`); the intermediate `running`
  state is not used by the bridge. Operators driving tasks manually
  via `relix-cli task update --status running` use it; the canonical
  bridge path skips it.

### The bridge's `MeshClient` auto-reconnects on transient drops

(Formerly a documented limitation; now **closed** — see git history.)
The bridge holds an alias → `Multiaddr` address book alongside the
alias → `PeerId` map. When `MeshClient::call` sees a transport-class
error (`DialFailure`, `ConnectionClosed`, `Timeout`, `io`), it re-dials
the original address once, waits briefly for the swarm to settle, and
retries the call. Live-verified by killing the memory peer mid-session:
the next chat fails with `retry after redial failed`; restarting the
peer and re-issuing the chat succeeds without a bridge restart.
Controller keys are persistent on disk so `PeerId`s are stable across
peer restarts; the cached mapping stays valid.

What's still **not** handled: a peer whose Ed25519 key is regenerated
(by deleting `dev-keys/<run>-<node>.key` and restarting). The bridge's
cached `PeerId` would be stale and the redial would connect to a peer
with a different `PeerId`. The fix is "delete the bridge's cache too
(restart the bridge)"; documented behaviour, not silent failure.

### Discovery refreshes periodically

(Formerly a documented limitation; now **closed** — see git history.)
The bridge spawns a background task that re-runs `node.manifest`
against every peer in its address book every 60s, updating the
`ManifestCache`. A peer that comes online *after* the bridge will be
discovered within one refresh interval and become reachable via
`capability:<method>`. A peer whose capabilities change at runtime
(e.g. a node-type with a hot-swap registration; not currently used in
any built-in node) will also be picked up.

What's still **not** handled: peers whose Multiaddr changes (different
port, different host). The address book is populated from `peers.toml`
at bridge startup and is not refreshed. SIMP-007 keeps applying for
fully gossip-based discovery; the alpha refresh covers the in-`peers.toml`
case only.

### No gossip / DHT-based peer discovery

The libp2p Kademlia behaviour is **configured** in the transport
stack (`crates/relix-runtime/src/transport/rpc.rs`) and `bootstrap_kademlia`
is called once at controller startup, but there is no working
DHT-based peer-find or capability gossip in the alpha. Peer addresses
come from the static `peers.toml`. The DHT being present in the swarm
configuration is **not** the same as being useful.

### Static peer alias map (`peers.toml`) is still load-bearing

Even with capability discovery (M10), every peer the bridge talks to
must be in the `peers.toml` so the bridge has somewhere to dial.
`capability:<method>` routing chooses *between* aliases in that file;
it does not discover new peers from the network. SIMP-017.

### No standalone log rotation

`dev-data/<run>/{memory,ai,tool,bridge}.log` grow unbounded. The
audit log (`<run>-<node>/audit.log`) is the integrity-relevant one
and should be shipped off-host on any real deployment. The script
itself does not rotate.

### Provider `local` (Ollama / vLLM / llama.cpp) is not stress-tested

`-Provider local -BaseUrl http://...` works for the deterministic
prompts the alpha exercises. Local model failure modes (model not
loaded, context overflow, GPU OOM) surface as generic provider
errors; there is no graceful fallback.

### Tool node pool has no LRU eviction

The `PinnedClientPool` grows one entry per unique
`(hostname, validated_addrs)` route the flow visits. A soft cap of
256 emits a WARN; eviction lands in a follow-up if real workloads
push past that. Bound is operator-driven (the set of hosts your flows
actually fetch).

## Security gaps the alpha owns

### Manifests are not signed

`NodeManifest` is sent as plain CBOR. A peer can lie about its own
capabilities; the bridge trusts what it receives. Gate 2 wraps the
manifest in `Bundle(BundleType::NodeManifest)` and verifies against
the org root. Relevant for any deployment where mesh peers are not
all under one administrator.

### Identity bundles have one delegation level

The org root signs IdentityBundles directly. There is no Intermediate
Authority (IA) layer. Compromised org-root key = compromised mesh.
Mitigate by keeping the org-root secret out of any controller config
and using short-lived bundles. SIMP-002.

### No CRL or revocation gossip

The only way to invalidate an identity is to let it expire. Default
bundle lifetime from `relix-cli identity mint` is 24 hours. Tighten
it for higher-risk roles (`--hours 1`). SIMP-003.

### Tool node cross-host redirect window is narrow but not zero

The redirect `Policy::custom` re-runs the SSRF guard on every hop, so
a redirect to a forbidden IP or hostname is rejected pre-connect. But
once the guard validates a cross-host redirect target, reqwest
re-resolves and connects with the default OS resolver (no pin for the
new host) — there is a sub-millisecond window between policy check
and connect during which an attacker controlling DNS for the redirect
target could rebind. Per-hop pinning needs a custom hyper resolver,
tracked in [`tool-node-security.md`](tool-node-security.md). For
zero-window posture today, set `[tool] max_redirects = 0`.

### `tool.web_fetch` is GET-only and text-only

`POST` / `PUT` / `DELETE` are not exposed. Response bodies must
decode as UTF-8 and have a `text/*`, `application/json`,
`application/xml`, `application/xhtml+xml`, or `application/*+json`
content type. Bodies are read whole into memory subject to the
configured cap.

### No outbound mTLS / origin-side client auth from the tool node

The tool node verifies origin certificates via webpki, but does not
present a Relix-issued client cert to the origin. Use this when you
need bidirectional auth between Relix and an upstream service.

### Audit log is per-node, not federated

Each controller maintains its own hash-chained audit log
(`dev-data/<run>-<node>/audit.log`). Cross-node correlation is by
`request_id` / `trace_id` recorded in both the responder's audit
record and the caller's per-flow event log. There is no audit
aggregator; operators are expected to ship logs to a SIEM.

### No per-caller / per-method rate limiting

The policy engine is allow / deny only. Cost-class-aware throttling
(the `CapabilityDescriptor::cost_class` field exists for it) is not
implemented. A caller that floods the AI node will simply burn the
provider's per-key budget.

## Wire-format gaps

### `remote_call` args and returns are UTF-8 strings (SIMP-016)

The wire envelope itself is CBOR with full typing, but the alpha
keeps the SOL ↔ handler boundary as `String`-shaped to avoid
inventing a SOL type system for the alpha. Pipe-delim fields are the
per-method convention. Typed CDDL replaces this at Gate 2.

### Bridge template substitution is character-level (SIMP-018)

The bridge writes a rendered `.sol` file per request. The
substitution validator rejects `"`, `|`, and newlines so user input
can't escape the SOL string literal. This works but is not the same
as typed flow arguments — three characters are forbidden in user
input. `relix-web-bridge::validate::validate_input` shows the exact
rule.

### Streaming is provider-native at the AI node, bridge-chunked at the HTTP edge (SIMP-019, partial)

Every active provider — `mock`, `openai`-compatible (OpenAI /
OpenRouter / xAI / local), Anthropic, Gemini — now implements
`ChatProvider::generate_reply_stream` against the provider's
native streaming endpoint:

- OpenAI-shape: `/chat/completions` with `stream: true`; parses
  `data: {...}` SSE frames into `choices[0].delta.content` deltas.
- Anthropic: `/v1/messages` with `stream: true`; parses
  `content_block_delta` events with `delta.type = "text_delta"`.
  Extended-thinking deltas are intentionally skipped (the
  assistant-visible reply text only).
- Gemini: `:streamGenerateContent?alt=sse`; emits the incremental
  suffix over a "cumulative running total" wire shape.

What still isn't end-to-end: the bridge's `POST /chat/stream` and
the `stream:true` variant of `/v1/chat/completions` invoke the
SOL chat flow via the mesh's request/response transport, which is
single-shot today. The bridge therefore still receives a fully-
materialised reply from the AI node and slices it into SSE
chunks at the HTTP edge. Provider→bridge streaming pass-through
needs a streaming `remote_call` primitive on the mesh transport
(Gate 2 spec target). Operators who want per-token streaming
today must call the AI node directly through a flow that returns
the streamed text.

### OpenAI shim drops fields (SIMP-020)

`/v1/chat/completions` accepts the full request shape. The current
behavior:

- `system` messages are **preserved** and prepended as
  `[SYSTEM N]\n<content>\n\n` blocks before the last user message.
- `tools` / `tool_choice` / `function_call` fields and `role:"tool"`
  messages are **rejected with 400** (not silently dropped).
- `temperature`, `top_p`, `n`, `presence_penalty`, `frequency_penalty`,
  `max_tokens`, `logprobs`, `response_format`, ... (sampling and
  format controls) are accepted but not forwarded; handled provider-side.
- Multimodal `content` arrays (only text-string content is supported).

The shim is a translation layer to make Open WebUI work, not a full
OpenAI API. Full surface is in
[`streaming-and-openai-shim.md`](streaming-and-openai-shim.md).

### Bridge bearer token is loopback-scoped, not internet-grade auth

The bridge enforces a bearer token on all non-public routes (stored
at `~/.relix/bridge-token`). For loopback-only deployments (the
default) this is sufficient. However:

- The `Authorization: Bearer` header must match the token exactly —
  any other value receives 401.
- For deployments exposed beyond loopback, a reverse proxy with
  TLS + external auth is still required. The bearer token is a local
  shared secret, not a substitute for mTLS or OAuth.
- `/health` and `/dashboard` are public (no auth) by design.

### Dashboard admin login is a single local account

The React dashboard authenticates with a username/password operator
login layered on top of the bridge token (see
[`relix-dashboard-design.md`](relix-dashboard-design.md)). What it does
**not** do:

- **One admin, not multi-user.** There is exactly one admin credential
  per bridge (`dashboard-admin.json`, Argon2id hash next to the bridge
  token). No roles, no per-operator accounts, no SSO.
- **Sessions are in-memory.** A logged-in session rides an HttpOnly
  `relix_session` cookie (12h TTL) held in the bridge process. Restart
  the bridge and every operator must log in again — by design, but it
  means a busy operator is logged out on every deploy.
- **No online password reset.** A forgotten password is recovered
  **only locally** on the host: `relix dashboard reset-admin` (or
  `relix-web-bridge reset-admin`, or
  `scripts/relix-dashboard-admin-reset.{ps1,sh}`). It rewrites just the
  admin credential — never the data — and there is deliberately no
  network/unauthenticated reset surface. Restart the bridge afterward.
- **Protected APIs stay protected.** The SPA shell (`/dashboard`) is
  public, but `/v1/tasks`, `/v1/spine/*`, `/v1/prime/*`, providers, etc.
  require the cookie (or the bearer). Before you log in — or after the
  session lapses — those calls return **401**; the dashboard now routes
  that to the login screen ("Your session expired — sign in again")
  instead of broken cards. A 401 on those routes is auth being
  **enforced**, not the spine being down — `relix dashboard doctor`
  distinguishes the two.

### Dashboard mobile shell + command palette are in, with honest gaps

The React dashboard now has a mobile shell and an operator command palette
(`relix-dashboard-design.md` §2/§12):

- **Mobile:** below 880px the 232px rail becomes an **off-canvas drawer**
  opened from a fixed mobile top bar's menu button, dimmed behind a tap-to-close
  scrim, and closed automatically on navigation. The drawer renders the **full**
  sidebar (grouped labels + sign out), so sign out stays reachable and the
  active route stays highlighted. Desktop layout is unchanged.
- **Command palette (⌘K):** a dependency-free, keyboard-accessible palette
  mounted once in the shell, opened with Ctrl/⌘+K or a topbar/mobile button. It
  **only navigates** to existing routes (Ask Prime, Command Center, the rail
  destinations, Action Center) or signs out — it performs **no backend
  mutation** and creates no work objects.

What it does **not** do yet:

- **No fixed bottom nav.** Design §2 also calls for a phone bottom nav
  (Home / Issues / Create / Agents / Inbox) and edge-swipe-to-open; only the
  drawer + menu button ship. The drawer covers the same destinations.
- **No tenant/company switcher.** Design §3 puts a tenant switcher at the top of
  the rail, but the dashboard is a **single local admin / single tenant** today
  (see "Dashboard admin login is a single local account" above) — there is
  nothing to switch between, so the switcher is deferred until multi-tenant
  operator state exists. The palette is the keyboard-first quick-jump in its
  place.
- **No automated visual verification.** The repo has **no Playwright / headless
  browser tooling**, and none was added for this (no heavy dependency for a
  screenshot). The mobile shell + palette are verified by a clean production
  build (`tsc -b` + `vite build`) and the dist-parity gate
  (`scripts/check-dashboard-dist.ps1`) only — **not** by rendered-pixel
  regression at desktop/mobile widths. Cross-browser/device rendering is a
  manual check until a browser-test harness is justified.

## Provider gaps

### `gemini` provider is a placeholder

`-Provider gemini` produces an AI node that returns clean errors
(not a real Gemini call). Tracked; will land when the Anthropic and
Gemini providers share a cleaner abstraction.

### One model id per AI node

The `[ai] model = "..."` field on the AI node is one default; the
bridge exposes `relix-<provider>` as the model picker entry. There is
no multiplexing of multiple models on one AI node — run a second AI
controller with a different config if you need both.

## SOL VM gaps

### Synchronous `remote_call` only (SIMP-001 / SIMP-014)

`remote_call` blocks the VM thread. The host bridges to async libp2p
via `tokio::task::spawn_blocking` + `Handle::current().block_on(...)`.
The flow can't issue concurrent calls.

### No `Inst::FlowArg` (SIMP-018)

A SOL flow takes no first-class arguments. The bridge does template
substitution and writes a rendered file per request. CLI `flow-run`
takes a `.sol` file with no arguments at all.

### No durable replay / no flow snapshots (SIMP-005)

The per-flow event log records every `RemoteCall*` event with hash
chaining, which is the property the replay-equivalence property test
(SIMP-008) is supposed to assert at Gate 2. Today the log is
write-only for audit.

### Hand-written flows; no SolFlow editor (SIMP-011)

Every flow under `flows/` is hand-written. There is no visual
authoring surface in the alpha.

## CI and quality gaps

### `cargo deny` is wired but not enforced in CI (alpha policy)

The `deny.toml` exists and `cargo deny check` runs cleanly locally,
but the alpha CI matrix doesn't fail PRs on `cargo deny` regressions
yet.

### No fuzz coverage (SIMP-012)

Wire format and SSRF parser are obvious fuzz targets and have none in
the alpha. Property-test coverage of codec determinism exists; fuzz
ships after Gate 2.

### Conformance tests exist but are alpha-narrow

`conformance/` holds wire-format vectors. Cross-language interop
testing (the test that proves the protocol is portable) is not in
scope until Gate 2.

## Platform gaps

### Windows-specific cleanup quirk

`scripts/relix-mesh-up.ps1` intercepts Ctrl-C and stops only the
PIDs it spawned. If the launching PowerShell process is *itself*
killed externally (not Ctrl-C), the script's `finally` doesn't run
and the children orphan — they have to be cleaned up manually.
Documented in
[`operator-guide.md`](operator-guide.md#stopping-the-mesh-safely).

### Open WebUI in Docker on Linux

On Linux Docker, `host.docker.internal` does not resolve by default.
Use `--add-host=host.docker.internal:host-gateway` when starting the
Open WebUI container, or use `--network=host` and `127.0.0.1`.

## How to think about all this

The alpha exists to prove the architecture — peer-native nodes, SOL
orchestration, per-call admission, audit, SSRF-guarded external
actions — works end to end. It is **not** trying to be production-
grade in any single dimension. Every gap above either:

1. has a clear path to closure in a later milestone, **or**
2. is a deliberate scope cut so the alpha could ship a coherent
   architecture rather than a 1.0 in one feature area.

If you find a gap that isn't in this document, that's a documentation
bug — please file it.

## See also

- [`security.md`](security.md) — what the admission pipeline does
  enforce.
- [`tool-node-security.md`](tool-node-security.md) — full SSRF /
  DNS-pin / redirect model with the exact remaining windows.
- [`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md)
  — every SIMP, with rationale and resolution gate.
