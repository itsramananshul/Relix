# Relix Execution & Issue Model — Deep Design

> **Companion to `docs/relix-company-model.md`.** This goes deep on the two most mechanically-involved phases: **Task → Issue** (the work object) and the **heartbeat / assignment + supervisory loop** (the execution engine). They're covered together because the loop operates *on* the issue.
>
> **Ideas and mechanics only — no code.** This is the stable intent layer; the code may change. Every "Paperclip does X" below was verified against a complete file-by-file read of Paperclip's execution core (the ~10K-line heartbeat engine, the ~6K-line issue service, the recovery layer, budgets/costs, and the workspace substrate). Every "Relix should do Y" is grounded in Relix's existing coordinator (the durable Task ledger, delegation executor, cron, and the signed admission pipeline).
>
> **The honest headline:** Relix already has most of the *parts* (a durable task ledger with attempts/events/edges, delegation with depth caps, cron, a signed-and-audited dispatch path). What it does **not** have is the Paperclip-shaped *assembly* of those parts: a single wakeup entry point with coalesce/defer, an atomic per-issue checkout lock, per-agent concurrency slots, and the conservative supervisory/recovery loop. This doc specifies that assembly.

---

## 0. How this maps to the roadmap

- **Part 1 (the Issue object)** is the deep design for roadmap **Phase 1** (evolve Task → Issue).
- **Parts 2–5 (the loop, supervision, budget, autonomy knobs)** are the deep design for roadmap **Phase 3** (the heartbeat/assignment loop).
- **Part 6 (workspaces)** touches a later phase but is included because the loop depends on it.
- **Part 7** lists the execution-specific decisions to lock before building.

---

# PART 1 — THE ISSUE OBJECT (Task → Issue)

## 1.1 What changes from a Relix Task today

Relix's coordinator Task is a **durable execution record**: it has a status machine (`pending → running → completed/failed/interrupted/cancelled/paused/frozen/awaiting_input`), append-only events, attempts (with lineage), todos, and edges (`delegated_to`, `retried_from`). It is created mostly as a *side effect* of chat or delegation, and it is bookkeeping — not something you author and assign.

A Paperclip-style **Issue** is the same durable record **grown up into the product object**: it adds a single assignee, a board status, a comment thread (the conversation), sub-issues, attached documents, first-class blockers, goal/project links, and a checkout lock. The Task ledger underneath (attempts, events, runs) stays — it becomes the issue's *execution history*. So this is one object leveled up, not two objects.

## 1.2 The fields an Issue gains (conceptually — not a schema)

From the verified Paperclip model, an Issue carries, beyond what a Task has:

- **Identity & ancestry:** a human identifier (e.g. `REL-42`, allocated from a per-company counter), a **single assignee** (one agent *or* one human, never both), a **parent issue** (sub-issue tree), a **project** link, and a **goal** link. Goal/project give the "why" ancestry.
- **Board status:** `backlog → todo → in_progress → in_review → done`, plus `blocked` (a side state) and `cancelled` (terminal). This is the kanban vocabulary.
- **The conversation:** a comment thread (human + agent + system notes), which *is* the communication channel — there is no separate chat.
- **Work artifacts:** attached **documents** (keyed, e.g. `plan` / `design`, revisioned, lockable), attachments, and **work products** (PRs / preview URLs / deliverables).
- **Dependencies:** first-class **blockers** ("this is blocked by those issues").
- **The execution lock (the heart of no-double-work):** a *checkout run* (who owns execution) and an *execution run* (which run is live right now), an *execution-agent-name key*, and a *locked-at* timestamp. (Paperclip distinguishes `checkoutRunId` = "who owns the right to execute" from `executionRunId` = "which run is live"; Relix should adopt the same two-pointer split.)
- **Cost attribution:** a **billing code** (so cross-team work bills to the requester) and a **request depth** (how many delegation hops from the original ask — Paperclip clamps this to a high bound, e.g. 1024; Relix's delegation today caps at 3, which is a *policy* choice we can keep or relax).
- **Execution policy/state:** an optional review/approval stage machine and a monitor (see 1.9).
- **Origin metadata:** how the issue was created (`manual`, `routine_execution`, a recovery origin, etc.) — used to dedupe auto-generated issues via uniqueness guards.

## 1.3 Status model + the transition philosophy (a deliberate Paperclip choice worth copying)

Paperclip does **not** enforce a rigid transition graph. Its `assertTransition` only checks the *target* is a valid status; the real constraints are **guards and side-effects** at the moment of update:

- `in_progress` requires an assignee and no unresolved blockers.
- **Leaving `in_progress` or changing the assignee atomically clears the checkout + execution lock** (so the next agent can claim it cleanly).
- Entering `done`/`cancelled` stamps the completion/cancel time; leaving them clears it.

This "permissive target-validation + guarded side-effects" approach is simpler and more robust than a hardcoded edge graph, and Relix should adopt it. The board UI still *shows* a sensible flow and rejects invalid drags with a toast, but the server's invariant is "the guards hold," not "this exact arrow is allowed."

A second Paperclip rule worth copying: **status is set by checkout/lifecycle, not poked directly.** An agent enters `in_progress` by *checking the issue out*, not by patching status. "Please review" + assigning yourself is **not** a valid review path — `in_review` requires a real reviewer (a human owner, a typed review participant, a linked approval, a pending interaction, or a scheduled monitor). This is what keeps the board honest.

**Closing the review tail (`in_review → done`).** A completed Shift opens its *run* review (`done` + `pending_review`) and parks its Brief in `in_review`; the Brief reaches board `done` only when the run is **accepted** and a clean **`run.apply`** integrates it (the review-to-done — see company-model §12.5B/§12.6). That accept + apply is a **human's by default**. Under two SEPARATE, default-OFF standing grants (**Prime Shift Disposition v1**, company-model §12.5G — `prime.run.review_accept` and `prime.run.apply`), the autonomous Prime loop may close this tail on the Board's behalf for a run in the candidate Mandate/proposal's **own** Brief set, through the EXACT existing review/apply paths and all their eligibility/baseline/conflict/artifact safety — never a hand-rolled copy, never combined into one power, and a conflicted/failed apply records `blocked` and **never** marks the Brief `done`. With neither grant, the gate stays a human's exactly as before.

## 1.4 Single assignee + atomic checkout (the no-double-work lock)

This is the most important mechanic in the whole product, and Paperclip's implementation is worth reproducing precisely (in spirit):

**Checkout is a single conditional update that only succeeds if the issue is claimable.** In prose: "set assignee = me, status = in_progress, checkout-run = my run, execution-run = my run — *only where* the current status is one of the expected set (todo/backlog/blocked/in_review) AND the assignee is null-or-already-me AND the execution-lock is null-or-already-mine." If zero rows match, it's a **409 conflict** — the agent backs off and picks other work. **Never retry a 409.**

Three refinements Paperclip adds that Relix should adopt:

- **Stale-run adoption.** If the issue shows a *different* checkout run but that run is already terminal/dead, the new agent may adopt it (the prior owner crashed). This prevents a dead run from permanently locking an issue.
- **Idempotent self-ownership.** If the caller's own run already holds the lock, checkout returns the issue (no error) — so retries are safe.
- **Release & admin-force-release.** Release clears the lock and returns the issue to `todo` (only the assignee or its checkout run may release). An admin force-release clears the lock without changing status (operator escape hatch).

**Relix mapping:** Relix's coordinator already does single-active-execution on tasks (the delegation executor uses a semaphore and the issue/task has execution state). What's missing is the *formal two-pointer checkout lock on the issue* and the *conditional-update claim*. Relix's signed admission pipeline is unaffected — checkout is just an API the assigned agent calls (carrying its run id), gated by the existing permission engine.

## 1.5 Sub-issues, the parent/child tree, and request depth

A manager/orchestrator breaks work down by **creating child issues**. Paperclip's `createChild` is the canonical mechanism and it does three things Relix should copy:

1. **Inherits ancestry:** the child takes the parent's project, goal, and workspace unless overridden. (So a worker's issue automatically traces to the same goal and runs in the same checkout as its parent.)
2. **Increments request depth:** child depth = max(parent depth + 1, requested) — the delegation-hop counter, clamped to a bound.
3. **Caps fan-out:** a single helper call can't spawn more than N children (Paperclip uses 25), preventing runaway decomposition.

Optionally, a child can be made a **blocker of its parent** ("parent waits until child is done"). This is how a manager says "I can't finish until my worker finishes."

**Relix mapping:** Relix already has parent/child task **edges** (`delegated_to`) and a delegation **depth cap** (3, enforced twice via an independent ancestor walk — a stronger guard than Paperclip's clamp). So the tree + depth machinery exists; what's new is "create child issue + assign + inherit goal/project/workspace" as the first-class delegation act, replacing the current delegate-spawn-then-ai.chat path.

## 1.6 Blockers & dependency readiness (first-class, with exact rules)

Dependencies must be **first-class**, not prose. The verified rules:

- **Replace-semantics:** setting an issue's blocker list *replaces* the whole set (send an empty list to clear all). Self-blocks and cycles are rejected.
- **Only `done` resolves.** A blocker resolves a dependent only when the blocker reaches `done`. A **`cancelled` blocker stays unresolved** — the dependent waits until an operator explicitly removes/replaces the relation. (This is a deliberate safety choice: a cancelled prerequisite is *not* a green light.)
- **Readiness** = every blocker is `done` *and* (Paperclip's extra refinement) every done blocker's **workspace has finalized** its sync-back (the `workspace_finalize` barrier — see Part 6). Until then the dependent is held.
- **Auto-wakes** (the supervisory engine, Part 3): when an issue becomes `done`, every now-unblocked dependent's assignee is woken (`blockers-resolved`); when *all* of an issue's children reach a terminal state, the parent's assignee is woken (`children-completed`, with a digest of what each child produced).

**Relix mapping:** Relix's task edges include a reserved `blocked_on` edge type but no first-class blocker wakes. This is net-new: add the blocker relation + the readiness computation + the two auto-wake reasons. It's the engine of the supervisory loop (Part 3).

## 1.7 Exactly-once plan decomposition (the planner's core tool)

This is the mechanism that makes the planner pattern *safe*: an approved plan becomes a set of child issues **exactly once**, even if the planner's run crashes and retries.

Paperclip's design, worth reproducing exactly:

- A **decomposition claim** is keyed by `(source issue, accepted-plan revision)` and carries a **fingerprint** (a hash of the requested children, normalized so cosmetic differences don't matter), the list of child ids created **so far**, and an owner.
- Creating children is a **resumable loop**: each iteration creates the *next* child (by cursor position in the list) in its own transaction and appends its id. A retry that died mid-way **continues from the cursor** — never re-creating already-made children.
- A second run with a **different** fingerprint for the same plan revision is rejected (you can't fork the plan); a second run with the **same** fingerprint resumes or no-ops.
- Ownership is held by the original run while it's alive; only if that run is terminal can another run take over.

**Relix mapping:** net-new, but it's a small, self-contained primitive. It's what lets "the CEO/planner accepts a plan → it becomes assigned issues" be crash-safe and non-duplicating — essential for autonomy. **Ownership/takeover in Relix is honestly weaker than Paperclip's "while the run is alive" rule, because the accept is realized differently:** the `suggest_tasks` accept is an **operator-driven, synchronous** materialization in the coordinator, so the claim's `owner` is the **accepter** (an operator/Founder subject), **not** a long-lived run with a heartbeat/lease (contrast the two-pointer Brief Claim, which *does* carry run-liveness). There is therefore no real run-liveness pointer to probe. The true exactly-once safety is the in-process **materialization lock + durable cursor + fingerprint**; on top of that, the resume path enforces a **conservative owner guard with stale-age takeover**: the **same** owner may always resume its `in_progress` claim; a **different** responder is **refused** while the claim is still **fresh** and may **take over** only once it is **stale** (untouched past `DECOMPOSITION_OWNER_STALE_SECS` ⇒ the owning process crashed and never resumed) or **terminal** (a `complete` claim no-ops for anyone). The fingerprint check runs **first**, so a forked plan refuses regardless of owner or staleness. In short: **owner here is operator-resumable with stale-age takeover, not a heartbeat-backed live run** — and correctness never depends on the guard, only the lock+cursor+fingerprint.

## 1.8 Issue documents (the plan lives as a document, not in the description)

Plans and designs are **keyed documents** attached to the issue (key `plan`, `design`, `notes`), with full revision history and locks — *not* free text in the description. Verified mechanics worth copying:

- Documents are revisioned with **optimistic concurrency** (you write against a base revision id; a mismatch is a conflict).
- Documents can be **locked**; writing to a locked document either errors or (a nice touch) **forks a new document under a new key** and tells you it redirected.
- The **plan-approval flow** ties documents to interactions (1.9): the planner writes the `plan` document, creates a "request confirmation" interaction bound to that *exact plan revision*, sets the issue to `in_review`, and waits. On acceptance, the plan decomposition (1.7) fires. **The CEO/planner does not build the team until the plan is accepted** — this is how we make the strategy gate real (it's only a prompt convention in Paperclip; see the main doc §10).

**Relix mapping:** Relix has a `documents` + `document_revisions` model in its memory/knowledge layer concepts but no issue-document link. This is net-new but straightforward, and it's the backbone of "planning" as a first-class mode.

**Status (v1) — IMPLEMENTED, with one explicit deviation.** Keyed, revisioned Brief documents ship as **Dossiers** (`task_documents`; capabilities `brief.dossier_author` / `brief.dossier_get` / `brief.dossier_latest` / `brief.dossiers`): keys (`plan`/`design`/`notes`/custom, validated `[a-z0-9_.-]`-style tokens), full append-only revision history, **optimistic concurrency** (write against the current latest via `expected_latest_doc_id`; a mismatch is a no-write `409` stale refusal), explicit **fork** (`forked_from_doc_id`), Chronicle events, and a 64 KiB body cap. **Document locking is now implemented** (`brief.dossier_lock` / `brief.dossier_unlock` / `brief.dossier_locks`, `POST/GET …/dossiers/lock|unlock|locks`): a per-`(Brief, kind)` lock where a write from anyone but the lock owner is **refused** (no silent overwrite), owner-or-nobody unlock, tenant-scoped, Chronicled. **Deviation from Paperclip:** a locked write is **refused** (the caller reloads / waits for unlock / explicitly `fork`s), it is **not auto-redirected to a new key** — that "locked-flag → writes redirect" nicety is deferred. There is no operator force-unlock and no lock lease/expiry in v1. The plan-approval flow (write `plan` Dossier → bind a `request_confirmation` to that exact revision → accept fires §1.7 decomposition) is implemented via the plan-package path.

## 1.9 Thread interactions (the answerable cards)

The issue thread renders three kinds of **structured, answerable prompts** the agent can raise:

- **suggest_tasks** — the agent proposes a tree of sub-tasks; you accept (all or a selected subtree) and they're created and assigned, or reject with a reason.
  - **Governed assignee hints (how "assigned" is realized).** Each proposed child may carry an **optional, explicit** assignee hint — either an **Operative id** (precise) or a **role** (friendly; resolved to the *oldest active same-Guild Operative* deterministically), never both. The hint is **never silently inherited from the parent**: an absent hint means the child opens **unassigned** (the default). On accept the hint is resolved and validated through the **existing assignment gate** — same-Guild + active, then the assign-Key authority check (`enforce_assign_key`; the Founder/operator is sovereign, an Operative is bounded by its `assign_scope`). The check runs **before any child is created**, so an unknown / cross-Guild / inactive / unauthorized hint refuses the **whole** accept with no partial materialization (the card stays open for a retry). Validated children are created already-assigned and chronicled (`brief.assigned` per child; the materialization event names the *assigned vs needs-assignment* split); unassigned children continue to surface in the Action Center as "Assign an Operative."
- **ask_user_questions** — radio/checkbox questions the agent needs answered.
- **request_confirmation** — a yes/no gate (used for plan approval and any "should I proceed?").

Each has a clean lifecycle (`pending → accepted/rejected/answered/cancelled/expired`), idempotency, a **continuation policy** ("wake the assignee when answered"), and **supersede-on-comment** (a pending confirmation expires if you just comment instead of clicking). This is what makes "the agent asks, you answer inline, it continues" feel native.

**Relix mapping:** Relix's coordinator has no interaction model. Net-new, but it's the surface that makes the chat-companion ↔ issue bridge work (the agent's questions become answerable cards rather than dead-end comments).

**Status (v1) — IMPLEMENTED, with explicit deviations.** All three kinds ship (`brief_interactions`; capabilities `brief.interaction_open` / `brief.interaction_create` / `brief.interactions` / `brief.interaction_respond` / `brief.interaction_cancel`, plus the `suggest_tasks` and plan-package paths): **`request_confirmation`** (yes/no, optionally **approval-bound** to an exact `plan` Dossier revision — an accept after the plan changed is refused as stale and the card `expired`), **`ask_user_questions`** (a prompt with optional radio/checkbox `choices`), and **`suggest_tasks`** (propose a bounded child-Brief tree → accept materializes Sub-briefs through the §1.7 exactly-once decomposition ledger, or reject closes it). Lifecycle is `open → resolved | rejected | cancelled | expired`, with **idempotency** on create (same `(brief, author, idempotency_key)` returns the existing card), an **idempotent respond** (a second answer is a typed refusal, never a silent overwrite), explicit **cancel** (idempotent on an already-cancelled card; refuses a decided one), and a **continuation policy** (answering/cancelling offers a supervisory wake to the assignee, best-effort). All tenant-scoped on the owning Brief. **Deviations from Paperclip:** (1) **supersede-on-comment** (a pending plain confirm auto-expiring when you merely comment) is **NOT** implemented for plain ask/confirm cards — only the *approval-bound plan confirm* expires, and only when its bound `plan` revision changes; (2) there is **no per-interaction `expires_at` TTL / clock-driven expiry** (expiry is event-driven via the stale-plan path only); (3) the `suggest_tasks` **governed assignee-hint** materialization is the pre-existing path (unchanged here).

## 1.10 Goal/project ancestry + cost attribution

- **Goal ancestry:** every issue resolves a goal (its own, its parent's, or its project's default) so the agent always sees the "why." Relix has no goals today — net-new (Phase 1).
- **Cost rollup:** costs attach to an issue (and agent/project/goal/run). A **recursive tree rollup** sums the cost of an issue *and all its descendants* — so a planner's issue shows the total cost of the whole effort it spawned. **Billing code** tags cross-team work so it bills to the requesting team even when the parent tree differs; **request depth** shows how deep a delegation cascade went.

**Relix mapping:** Relix already tracks cost per agent/issue/run (cost_events) and has a budget enforcer. The **tree rollup** and **billing-code/request-depth attribution** are net-new but build directly on the existing cost ledger.

## 1.11 Task → Issue: reuse vs net-new (honest)

| Issue capability | Relix has | Net-new |
|---|---|---|
| Durable record, attempts, events, edges, todos | ✅ (Task ledger) | — |
| Parent/child tree + delegation depth cap | ✅ (task edges, cap 3) | reframe as sub-issues |
| Single assignee | partial | the assignee field + XOR invariant |
| Board status + checkout lock | partial (single-active-exec) | the two-pointer checkout lock + conditional-update claim |
| Comment thread as the channel | partial (chat turns) | issue-as-thread |
| First-class blockers + readiness + auto-wakes | ❌ (reserved edge only) | **net-new — engine of supervision** |
| Exactly-once plan decomposition | ❌ | net-new (small primitive) |
| Issue documents (plan/design) + approval flow | ❌ | net-new |
| Thread interactions (ask/confirm/suggest) | ❌ | net-new |
| Goals + goal ancestry | ❌ | net-new |
| Cost tree rollup + billing code | partial (flat cost) | rollup + attribution |

---

# PART 2 — THE EXECUTION ENGINE (heartbeat / assignment loop)

This is what makes "assign it and it works." The whole engine is organized around **one entry point**.

## 2.1 The wakeup is the single entry point

In Paperclip, **nothing invokes an agent directly** — every trigger funnels through one `wakeup(agentId, options)` function. This is a load-bearing design choice: it's the single place where budget, agent-state, policy, coalescing, and locking are enforced, so there's no bypass. Relix should adopt the same chokepoint (and it already has the right instinct: its admission pipeline is the single enforcement point for *calls*; the wakeup queue should be the single enforcement point for *runs*).

## 2.2 Wake sources + the gate order (exact)

Four sources: **timer** (scheduled heartbeat), **assignment** (an issue was assigned / @-mentioned), **on_demand** (manual / UI / chat companion), **automation** (recovery / continuation / monitor).

Every wakeup passes these gates **in order** (verified):

1. **Context enrichment** — derive issue id, task key, wake-comment id, wake reason.
2. **Budget pre-check** — a hard-stop gate (Part 4). If blocked → write a `skipped` wakeup row (audited) and refuse.
3. **Agent-state gate** — paused / terminated / pending-approval → refuse.
4. **Heartbeat-policy gate** — a `timer` wake is dropped if the agent's scheduled heartbeat is off; a non-timer wake is dropped if the agent's "wake on demand" is off. (These are the autonomy toggles — Part 5.)
5. **Subtree pause-hold gate** — if the issue's subtree is paused, refuse (unless it's a verified interaction wake).

Every refusal writes an audited `skipped` row with a reason. This is the "no silent failures" contract at the engine level.

## 2.3 The issue-lock transaction: coalesce / defer / queue (the no-double-work guarantee)

When a wakeup targets an issue, everything happens inside a transaction that **opens by locking the issue row** (a `SELECT … FOR UPDATE`). Then the exact decision (verified):

- **Same agent already running this issue** → **coalesce**: merge the new context (e.g. the new comment) into the live run, record a `coalesced` wakeup, start **no** new run. (The running agent will see the new comment when it next reads context.)
- **A different agent** (e.g. someone @-mentions `@CTO` while the assignee is mid-run) → **defer**: record a `deferred_issue_execution` wakeup, start **no** run now. It will be promoted when the current run releases the lock. *This is the guarantee that a mid-run mention never starts a second agent on the same issue concurrently.*
- **No active run** → **queue**: insert a queued run and a queued wakeup.

After the transaction, the engine tries to start the next queued run for the agent.

**Relix mapping:** Relix's wakeup model today is poll-based (delegation executor) + cron + event. Adopting this transaction-with-coalesce/defer is the core net-new execution work. Relix's DB (Postgres via the coordinator's SQLite/pg) supports row locking, so the mechanism transfers directly. The `agent_wakeup_requests`-style queue with `coalesced`/`deferred` statuses is the shape to build.

## 2.4 Claiming a run (atomic) + lazy locking

A queued run becomes live via an **atomic claim**: "update this run to `running` *only where it is still `queued`*." Only one caller can win the row — that's the concurrency primitive. Before flipping, it re-checks the gates (agent invokable, budget, pause-hold, dependency readiness, staleness); any failure cancels the run instead of running it.

**Lazy locking (a refinement worth copying):** the issue's execution lock is **not** stamped at queue time — it's stamped only when the run actually transitions to `running` (guarded by "assignee is still me and lock is null-or-mine"). This avoids a queued-but-never-started run holding a lock.

## 2.5 The run loop (the ordered steps)

Once a run is claimed and live, the loop assembles everything the agent needs and dispatches. The verified order (compressed to the design-relevant steps):

1. **Auto-checkout** the issue (if the wake warrants it) — atomic, gated.
2. **Resolve the workspace** (project primary vs isolated worktree vs reuse; precedence issue → project → agent-default; see Part 6).
3. **Inject secrets** by reference — strip runtime-owned env, resolve adapter/project/routine secret refs; expose a *manifest* (not values) and redact secret values from logs.
4. **Inject skills** mentioned on the issue + the agent's runtime skills.
5. **Resolve the model profile** (normal vs cheap lane — the cheap lane is used for status-only recovery work).
6. **Resolve the session** (resume the prior session for this task key, or rotate to a fresh one on compaction / forced-fresh reasons).
7. **Assemble the prompt/context** — the task markdown (with goal ancestry), the wake payload (recent comments, the specific comment that woke it), and the continuation summary.
8. **Acquire an environment lease** and realize the execution target (local / ssh / sandbox).
9. **Mint a short-lived run credential** (the run JWT) so the agent can call the Relix API *as itself* for the run's duration, and **dispatch the adapter**, streaming output (redacted) to the log store + live events.
10. **Finalize:** parse usage/cost/session, set run status, post a run-summary comment, **release the lock and promote deferred wakes**, run liveness-continuation and successful-run-handoff checks, update the cost ledger, persist the session.

**Relix mapping:** Relix already does most of *steps 3–9* in its AI-node + flow-runner + credential-vault + run-JWT machinery — but driven by a flow, not by an issue-scoped run loop. The net-new work is the *loop scaffolding* (1, 2, 10) and wiring the existing pieces into it.

## 2.6 Concurrency (three layers)

1. **Many agents run in parallel** by default — each agent is independent. A CEO + planner + five workers all run at once. (No global lock.)
2. **Per-agent concurrency slots:** an agent runs up to `maxConcurrentRuns` (default 20, clamped 1–50) at once. Slot accounting is "count running, claim up to the available slots," serialized by a **per-agent start lock** (an in-process promise chain with a stale timeout) so two dispatch passes can't both claim the same slot.
3. **One run per issue:** the issue execution lock guarantees a single live run per issue; concurrency comes from *having many issues*, not racing one.

**Relix mapping:** Relix's delegation executor has a fixed concurrency semaphore (5). The design upgrades this to *per-agent* slots driven by the agent's runtime config, plus the per-issue lock.

## 2.7 Release & promote deferred wakes

When a run finishes, releasing the issue lock **promotes the oldest deferred wakeup** for that issue into a queued run (so the `@CTO` that was deferred mid-run now runs). The release also handles a few conservative recovery cases (see Part 3). This promote-on-release is what makes deferral safe: deferred work isn't lost, it's sequenced.

## 2.8 Execution: reuse vs net-new (honest)

| Engine capability | Relix has | Net-new |
|---|---|---|
| Signed, audited dispatch of a call | ✅ (admission pipeline) | — |
| Run record with attempts/events/logs | ✅ (task attempts + run log) | reframe as runs on issues |
| Secret injection, run JWT, session resume, adapter dispatch | ✅ (vault, run-JWT, AI node, sessions) | wire into the loop |
| Cron + delegation + channel triggers | ✅ | funnel into one wakeup |
| **Single wakeup entry point** | ❌ | net-new |
| **Coalesce / defer / queue on a locked issue** | ❌ | **net-new — the core** |
| **Atomic queued→running claim + lazy lock** | partial | formalize |
| **Per-agent concurrency slots + start lock** | partial (fixed semaphore) | per-agent, runtime-config-driven |

---

# PART 3 — THE SUPERVISORY LOOP (the planner / orchestrator pattern)

This is your exact "planner spawns workers, they report back, it assigns the next slice" vision. It is **entirely event-driven** — no agent ever busy-polls.

## 3.1 The loop

1. The planner reads the problem and writes a **plan document**; if you required it, it goes through plan-approval (1.8).
2. On acceptance, the plan becomes **child issues exactly once** (1.7), each assigned to the right worker, each inheriting the goal/project/workspace.
3. Independent children (no blockers) run **in parallel** across the workers; dependent children sit **blocked** until their prerequisite is `done`.
4. **The planner exits.** It does not sit and poll.
5. It is **re-woken** by one of two automatic reasons:
   - **children-completed** — when *all* of an issue's children reach a terminal state, the parent's assignee wakes with a **digest of what each child produced**.
   - **blockers-resolved** — when a prerequisite finishes, the now-unblocked issue's assignee wakes.
6. On waking, the planner reviews the results and assigns the next slice (more children) or marks the goal done. Loop to 3.

## 3.2 No busy-poll — this is a hard rule

The agent operating contract is explicit: *"use child issues for parallel/long work; do not busy-poll agents, sessions, or child issues waiting for completion."* The org tree + sub-issues + blockers + the two wake reasons **are** the orchestration engine. An agent that finishes its slice exits and gets re-woken; it never burns budget watching a loop. This is what makes a team of agents economical.

## 3.3 Conservative recovery (the invariants — copy these exactly)

Paperclip's recovery layer is deliberately cautious, and these invariants are *why* a company of agents doesn't spiral. Verified contract:

- **Never auto-reassign to a different agent for normal continuation.** A stranded issue is re-woken for its *same* assignee. Ownership only changes on *escalation to `blocked`*, and only to a deterministic chain (assignee's boss → creator's boss → creator → CTO/CEO), and only to a candidate that's both invokable and budget-clear.
- **Retry once, then escalate.** Dispatch/continuation retries are bounded (typically 1 attempt; transient-infra failures get ~3 with exponential backoff). After exhaustion → escalate to `blocked` / open a recovery action / surface to the Board. **Never loop.**
- **Comments are evidence, not liveness.** A successful run is *not* "done" just because it wrote comments/documents — the issue state must *also* record a real disposition. Work left `in_progress` with no live path is invalid and gets a "needs a next step" handoff.
- **Recovery never silently fixes.** The founding principle is "surface problems, don't silently complete them." Recovery work runs in a **cheap, status-only lane** that's forbidden from doing deliverable work or document updates.
- **Pause holds and budget hard-stops suppress all automatic recovery.**

**Relix mapping:** Relix's recovery today flips overdue running tasks to `interrupted` (a simpler version). Adopting the conservative contract (retry-once-then-escalate, never-auto-reassign, comments≠liveness, cheap recovery lane) is net-new but it's a *behavior contract*, not heavy infrastructure — much of it is "what the reconciler decides."

## 3.3b Diagnosis-driven escalation + the Inbox decision card (Relix refinement — beyond Paperclip)

Paperclip *classifies* failures but escalates with a terse, technical recovery record. Relix improves on this: when work can't self-heal, **diagnose the problem, explain it to the operator in plain language, and offer clean choices.** The flow is a two-stage triage:

**Stage 1 — Classify (instant, no LLM, free).** Bucket the failure:
- **Transient / timing** (started too early, dependency not ready, upstream rate-limit, adapter hiccup, timeout) → **auto-retry lane** (silent; don't surface to the operator).
- **Hard blocker** (missing credential, permission denied, budget exhausted, a genuine error that won't self-heal) → **escalation lane**.
- **Unsure** → escalation lane (when in doubt, ask).

**Stage 2a — Auto-retry lane (silent).** Re-wake the *same* agent with growing backoff, bounded by a **generous but finite cap**. (Infinite "try until it works" is disallowed — it risks a quiet budget-burning loop. If still failing after the cap → reclassify as a hard blocker and escalate.) Note: "started too early" is best *prevented* by the blocker/readiness model (the dependent only wakes when its blocker is `done` + finalized), so the auto-retry here is a safety net for genuine races, not the primary mechanism.

**Stage 2b — Escalation lane (the Inbox decision card).** Spawn a **cheap diagnostic pass** in the status-only recovery lane (cheap model, *forbidden* from doing deliverable work — diagnosis only). It reads the run logs + issue context and produces (1) a **plain-language root cause** ("the tester couldn't start because the staging DB-URL secret isn't set for this agent") and (2) a **recommendation** ("add the secret and retry, or block until configured"). Then it posts an **Inbox decision card** carrying the explanation + one-click choices:
- **Retry now** (re-wake the same agent)
- **Block** (with a reason — parks it)
- **Reassign** (hand to another agent — the *operator* may, even though the system never auto-reassigns)
- **Investigate** (opens the chat companion pre-loaded with the diagnosis)
- **Dismiss / false-positive**

The operator clicks; the choice drives the next step; everything is audited. **Only the escalation kind reaches the Inbox** — transient failures retry silently — so the Inbox stays signal, not noise. This wires the conservative-recovery contract, the Inbox, and the chat companion into one flow.

## 3.4 Runaway detection + silent-run watchdog (two safety nets)

- **Productivity review:** detects an agent churning unproductively on an issue (a streak of runs with no new comment, very long active duration, or high churn) and spawns a *review issue* assigned up the chain — and can **hold continuation** so the agent stops digging. This is the "agent went too deep / looping" guard.
- **Silent-run watchdog:** a running run that's been *silent* (no output) past a threshold (suspicious at ~1h, critical at ~4h) gets a review issue and, if critical, blocks the source issue on it — **without killing the process** (silence is a signal, not proof of failure). If the source issue already reached a terminal disposition durably, the watchdog folds the stale run (the "source-resolved fold").

**Relix mapping:** Relix has a confidence/judge layer but no productivity-review or silent-run watchdog. Net-new, lower priority (Phase 3+), but they're what make 24/7 autonomy safe.

---

# PART 4 — BUDGET & COST IN THE LOOP

Two enforcement modes, both verified:

## 4.1 Preflight hard-stop gate

A single check — "is this scope (company / agent / project) budget-blocked?" — is called at **every** decision point that would spend money: at wakeup enqueue, at run claim, at scheduled-retry, at liveness-continuation, at successful-run-handoff, and at every recovery owner-selection. If blocked → the wake is skipped (audited) or the run is cancelled. The gate checks money first (billed dollars), not raw tokens.

## 4.2 Reactive enforcement

Every cost event runs through an evaluator: at the **soft** threshold (default 80%) it raises an incident and notifies; at the **hard** threshold (100%) it resolves the soft incident, raises a hard incident **with a budget-override approval**, **pauses the scope**, and **cancels in-flight + queued work** for that scope. Resume is via the override approval (raise the budget and resume / resume once / keep paused). One open incident per scope-per-window prevents alert spam.

## 4.3 Cost rollup

Costs roll up the **issue tree** (an issue's cost includes all its descendants), so a planner's issue shows the whole effort's cost. Billing code attributes cross-team work to the requester; request depth shows cascade depth.

**Relix mapping:** Relix already has a budget enforcer (per-caller caps, soft/hard, alert engine) and a cost ledger. The net-new work is (a) wiring the preflight gate into the *new* wakeup/run decision points, and (b) the issue-tree rollup + billing-code attribution. The pause-and-cancel-scope hook already exists in spirit (the budget enforcer can reject/throttle).

---

# PART 5 — THE KNOBS THAT GOVERN THE LOOP (autonomy + permissions)

This ties the loop back to the company model. **None of the loop's behavior is hardcoded per agent** — it's governed by three editable things.

## 5.1 Autonomy = the agent's runtime heartbeat config

Three settings (verified) decide how autonomous an agent is:

- **scheduled heartbeat on/off (+ interval):** does the agent wake *itself* on a timer to scan its work? **Default off.** This is the "autonomous poller" switch (the CEO uses it; workers usually don't).
- **wake-on-demand:** does it spring to life when assigned an issue or @-mentioned? **Default on.** This is the "reactive worker" switch.
- **concurrency:** how many runs at once (default 20, clamp 1–50).

These map to the dashboard's per-agent "Autonomy" toggles (main doc §5.2-D). A reactive worker = heartbeat off + wake-on-demand on. An autonomous Prime/CEO operating mode = heartbeat on + interval.

**Two autonomy layers, both opt-in + bounded + default OFF.** The per-agent heartbeat above only *executes* already-assigned work (it never plans a team, orchestrates a Mandate, or authors strategy). The *company-orchestration* counterpart is the coordinator-level **autonomous Prime driver** (runtime-toggle default OFF, with `RELIX_AUTONOMOUS_PRIME` retained as a global env override; see `docs/current-limitations.md` and `docs/product-spine-implementation.md`): a timer that drives already-**approved** Prime work forward one safe governed step at a time — `propose_strategy` / `create_team_plan` / `orchestrate_assign_ready` through the same `prime.advance` path the operator click uses, and starting ready Briefs through the same shared guarded run pipeline (re-imposing the autonomous budget hard-stop that the sovereign manual `prime.start` skips) — for an approved proposal through the existing `prime.start` path, and for a **bare Mandate** (one reached `ready_to_start` with no owning proposal) by starting its ready same-tenant Briefs itself (`start_mandate`) through the same `heartbeat::preflight_and_spawn_with_trigger` → `prepare_claimed_run` → `execute_ready` chokepoint (claims, adapter probe, durable run ledger, board advancement, Chronicle `prime.autonomous_mandate_start`), stamped as an autonomous/heartbeat-trigger run rather than dashboard `manual`, tenant-scoped and budget-gated (no second run system is invented). **Prime Strategy Drafting v1:** when a Mandate has no strategy yet, the driver now **drafts** a strategy doc (from the Mandate's fields + the Guild's active work roles) and proposes it via the existing `mandate.strategy.propose` path — but this is a *draft only*: the strategy lands `proposed` and still requires approval before team planning unlocks; an already-`proposed`/`approved`/`rejected` strategy is never overwritten (so a human rejection is honoured). The strategy *body* is deterministic by default and **opt-in model-authored** under `RELIX_PRIME_LLM_STRATEGY_DRAFT` (**Prime Strategy Authoring v1**): in the autonomous/manual-tick loop a model may author the body text from a bounded, secret-free snapshot, re-validated + sanitized server-side and still only ever *proposed* — never approved or executed by the model; on unavailable/invalid/disabled output it falls back to the deterministic draft with honest `strategy_ai_mode` provenance, and the explicit one-click `prime.advance` route stays deterministic. **Prime Executive Prioritization v1:** candidate discovery/order is no longer fixed-deterministic-only — behind a third default-OFF switch (`RELIX_PRIME_LLM_PRIORITIZATION`) a model may choose only the *order* in which a bounded tick spends its action budget across the already-computed legal candidates (or hold the queue), validated to the offered candidate keys only (it cannot invent a candidate, add or widen an action, or bypass any gate); invalid/unavailable output falls back to the deterministic discovery order with honest `priority_ai_mode`/`priority_rank` provenance. This is *queue prioritization among already-legal candidates*, not freeform goal invention or tool-calling — so with `RELIX_AUTONOMOUS_PRIME_MAX=1` the loop can elevate the genuinely-most-important legal candidate above the deterministic first. **Prime Orchestration Authoring v1:** the orchestrated Brief tree's *text* is no longer mechanical-only — behind a fourth default-OFF switch (`RELIX_PRIME_LLM_ORCHESTRATION`) a model may author the titles / dossiers / checklists of the already-computed Brief skeleton (parent / role tracks / subject executions) on the autonomous/manual-tick orchestrate step, validated to the offered role/subject keys only (it cannot invent a role, agent, Brief id, assignment, dependency, or gate); a newly-created Brief gets the model text while an existing/hand-edited title is never clobbered, placeholder tracks stay deterministic, and invalid/unavailable output falls back to the deterministic titles + dossiers with honest `orchestration_ai_mode` provenance. The **direct one-click** `mandate.orchestrate` stays deterministic. **Strategy approval is not automatic by default** — that approval is a human's, **unless** the autonomous Prime loop is effectively ON **and** the Board has granted the bounded `prime.strategy.approve` standing authority for the Guild, in which case the same loop approves the *proposed* strategy through the existing `mandate.strategy.approve` handler (a separate per-candidate action from drafting, so a bare Mandate drafts on one tick and approves on the next). A **strategy rejection stays final** — the store only flips `proposed` → `approved`, so a rejected (or missing) strategy is never auto-approved or re-proposed; only a human proposing a fresh strategy reopens the gate. It is bounded per tick (`RELIX_AUTONOMOUS_PRIME_MAX`, clamp 1..=10), idempotent, tenant-safe, and **by default never auto-approves a strategy / proposal / hire / spawn / budget / Clearance gate** — those remain human unless a separate bounded standing-authority grant explicitly covers that approval category (and budget is never delegated). So autonomous Prime/CEO operation now has two complementary, separately-gated switches: per-agent heartbeat (execute assigned work) and autonomous Prime (advance approved company work, including drafting a strategy proposal — and, only under an explicit grant, approving the proposed strategy); a model may author the *body* of a proposed strategy when `RELIX_PRIME_LLM_STRATEGY_DRAFT` is on, but the strategy is still only *proposed* and its **approval** remains a governed status flip the model never performs. **Manual Autonomy Tick v1:** the autonomous Prime driver is also operator-wakeable on demand — `prime.autonomy_tick_now` (`POST /v1/spine/prime/autonomy/tick`, a **Run Prime now** button on Settings) runs **exactly one** bounded tick for the operator's Guild and returns the per-candidate tick records (what it considered / advanced / started, with each record's phase / action / outcome / reason), so autonomous Prime is legible rather than a mysterious background timer. The **background runtime toggle controls the timer; the tick-now wake-up is an explicit operator action for one Guild and does NOT require the timer switch to be ON** — but it grants no new authority and is the *same* governed path: operator/admin-only, tenant-scoped to the caller's own Guild (never all Guilds, even under the env override), and still obeying standing grants, the autonomous-start budget hard-stop, Rig readiness, and the per-tick `RELIX_AUTONOMOUS_PRIME_MAX` bound.

## 5.2 The permission gate already governs *what* an agent may do

Every call an agent makes in the loop still passes Relix's existing five-phase agent gate (status → surface → risk ceiling → deny/allow categories → approval-required categories) and the policy engine. So "can this planner spawn agents / assign work / use this tool / spend money" is enforced by the gate, not by the loop. The loop just *runs* the agent; the gate *bounds* it.

## 5.3 The planner is configured, not coded (the key insight, restated)

A "planner" is not a code path. It is an agent whose **instruction bundle** (its markdown job description) says "read the problem, write a plan, decompose into child issues, assign workers, review on children-completed, assign the next slice" — plus the **permissions** to assign work (scoped to its subtree) and optionally spawn agents, plus the **autonomy** to wake on assignment. The loop (Parts 2–3) provides the *mechanism* (children-completed wakes, exactly-once decomposition, the supervisory re-wake); the instruction bundle provides the *behavior*; the toggles provide the *guardrails*. This is why "ask the CEO to build me a planner that does X" works without touching code, and why the chat companion can author an org by conversation.

---

# PART 6 — WORKSPACES & RUNTIME SERVICES (the substrate the loop runs on)

Brief, because it's a later phase, but the loop depends on it. The verified three-layer model:

- **Project workspace** — the durable on-disk checkout (the repo). One primary per project.
- **Execution workspace** — the per-issue runtime checkout. Modes: shared (the project primary), **isolated** (a git worktree on its own branch), operator-branch, adapter-managed, or cloud-sandbox. Precedence: issue setting → project policy → agent default. Worktrees are created/reused/cleaned up with careful guards (never delete the project primary; reattach a missing worktree locally).
- **Environment lease** — *where* it runs (local / ssh / sandbox / plugin driver), leased per run.
- **Runtime services** — long-lived companion processes (dev servers, preview URLs) started in a workspace, reused by a stable key, health-checked, and stopped by policy.

Two contracts worth noting: the **no-remote-git contract** (the local checkout is authoritative; remote workspaces are synced from/to it, never via remote git push) and the **`workspace_finalize` barrier** (a dependent waits not just for its blocker to be `done`, but for the blocker's workspace to finish syncing back — so it sees the finished work).

**Relix mapping:** Relix has no workspace/worktree model today (agents run in a cwd). This is a substantial later phase, but the design above is the target. It is *not* required for Phases 1–3 (issues + the loop can run in a simple shared cwd first); isolated worktrees are an enhancement.

---

# PART 7 — Execution-specific decisions (DECIDED 2026-06-02)

1. **Two-pointer lock — LOCKED: yes.** Adopt the split: a *checkout run* (ownership) + an *execution run* (live). Cleanly handles "owned but not currently running" and crash recovery.
2. **Request-depth cap — LOCKED: raise to match-or-exceed Paperclip's high bound (≥1024).** Treat it as a runaway *safety backstop*, not a product limit — a real org tree goes deeper than 3. Budget + approval gates are the real spend controls.
3. **Cancelled-blocker rule — LOCKED: yes, cancelled blockers stay unresolved** until a human explicitly removes the relation. A cancelled prerequisite is not a green light.
4. **Recovery — LOCKED: diagnosis-driven retry-once-then-escalate + never-auto-reassign + comments≠completion, with a plain-language Inbox decision card.** See §3.3b below for the full design (this is a Relix refinement *beyond* Paperclip).
5. **Workspace isolation — LOCKED: per-project policy, shared by default, isolated *local git worktree* as a cheap opt-in, cloud sandbox deferred.** Key clarification that resolved the cost worry: a **local git worktree costs ~nothing** (local disk + a few seconds of setup; *zero* extra LLM/API/compute) — only **cloud sandboxes** (separate VMs) cost real money, and those are deferred. So "both shared and isolated" is a *per-project knob* (issue → project → agent-default precedence), not an either/or. Ship Phases 1–3 on shared; add the isolated-worktree toggle as a fast-follow. **Reuse policy — LOCKED: reuse-on-continuation, fresh-on-collision.** When the same agent continues the same line of work (a follow-up run on the same issue / building on top of prior work that has stopped), **reuse** the existing workspace — no point making a new one. When the new work would **collide** with work already in progress or already done (a parallel/different line on the same repo), **create a fresh** isolated workspace. In practice this falls out of keying the workspace to the *issue / line of work*: the same issue's runs reuse its workspace; a new or parallel issue gets its own (which inherently avoids collision). The only real cost of a fresh workspace is dependency-install time, which reuse avoids.
6. **Where the loop lives — LOCKED: extend the coordinator** (it already owns the task ledger, delegation executor, and cron — the wakeup engine is the natural next layer there).

---

# PART 8 — Minimal build order within Phases 1 & 3

A buildable slice order (each step leaves something usable):

**Phase 1 (Issue):**
1. Issue = Task + single assignee + board status + the two-pointer checkout lock + the conditional-update claim/release. (Now you can create, assign, and atomically check out an issue.)
2. Sub-issues (create-child with goal/project inheritance + request depth) + first-class blockers + readiness.
3. Comment thread as the channel + thread interactions (ask/confirm/suggest).
4. Issue documents (plan/design) + the plan-approval flow.
5. Goals + goal ancestry + cost tree rollup + billing code.

**Phase 3 (Loop):**
6. The single wakeup entry point + the gate order, funneling cron/assignment/on-demand into it.
7. The locked-issue transaction: coalesce / defer / queue.
8. Atomic claim + lazy lock + per-agent concurrency slots + start lock.
9. The run loop scaffolding wrapping Relix's existing dispatch/secret/session/JWT machinery.
10. Release-and-promote + the two supervisory wakes (children-completed / blockers-resolved).
11. Conservative recovery (retry-once-then-escalate, never-auto-reassign, comments≠liveness) + budget preflight at every decision point.
12. (Later) productivity review + silent-run watchdog + isolated worktrees.

---

*This is the execution-spine design. It is grounded in a complete read of Paperclip's engine and Relix's existing coordinator. The next concrete step is to pick the Phase-1 slice (step 1 above) and design its exact data shape against Relix's coordinator schema.*
