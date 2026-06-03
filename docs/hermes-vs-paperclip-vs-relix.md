# Hermes vs Paperclip vs Relix — what's good where, and what Relix should take

> **Companion to the Relix design docs.** Ideas-only. Grounded in a complete read of all three systems: Hermes (the full Python codebase — the learning loop, memory, agent loop, delegation/execute_code, the six terminal backends, kanban, cron, the trajectory pipeline), Paperclip (the full TS codebase — the company/issue/governance model), and Relix (its own mesh + coordinator).
>
> **The one-paragraph framing.** These three systems live at **different layers** and are complementary, not competing:
> - **Hermes is the *employee*** — one extraordinarily capable, *self-improving* agent. Its depth is in a single agent: it learns skills from experience, models who you are, runs anywhere (even serverless), and writes code that calls its own tools. It has almost no notion of "a company of agents."
> - **Paperclip is the *company*** — the control plane that organizes *many* agents toward goals: an org chart, issues, governance, budgets, approvals. Its agents don't learn; the company organizes them.
> - **Relix is the *secure building + the rules*** — a signed, audited, policy-gated mesh the agents run on. And (per the design docs) Relix is now also becoming the *company*.
>
> **Relix's opportunity is to be all three at once:** the secure mesh (its own), the company (from Paperclip), staffed by world-class *self-improving* employees (from Hermes). The design docs already adopt the Paperclip company model. This doc is about what to fold in from Hermes.

---

## 1. What each system actually is

| | Hermes | Paperclip | Relix |
|---|---|---|---|
| **Core metaphor** | A self-improving personal agent ("the best employee") | A control plane for a company of agents | A secure agent mesh (+ now a company) |
| **Scope** | Depth in ONE agent | Breadth ACROSS agents | The substrate + (now) the org |
| **Deployment** | Single-user, single-host, local daemon, SQLite | Server / multi-company control plane | Distributed signed libp2p mesh |
| **Security model** | Command approval + container isolation + memory fencing (protects one user) | Company-scoping + permissions + approvals (governs a company) | Cryptographic identity + responder-enforced policy + hash-chained audit (secures a mesh) |
| **Learning** | **Closed self-improvement loop** (skills + memory + trajectories) | None — agents don't learn | Has the primitives (inherited from Hermes) but loop not closed |
| **Multi-agent** | Flat SQLite kanban, assignee-string routing | Org tree, issues, goals, governance | Delegation + coordinator ledger (+ adopting Paperclip's model) |

---

## 2. The standout strength of each

- **Hermes's superpower: it gets *better* over time, on its own.** A closed create→use→curate→retrieve loop turns experience into a curated, self-pruning skill library and a deepening user model — and it captures its own runs as training data for the next model. No other system here learns.
- **Paperclip's superpower: it makes "run a company of agents" legible and governed.** The work-object spine (Company→Goal→Project→Issue→Run), the org chart, and the approval/budget governance let one human oversee many durable agents working toward goals.
- **Relix's superpower: nothing runs unverified.** Every cross-node call is identity-signed, policy-gated on the responder, and hash-chain-audited. It's the only one with a real security substrate.

---

## 3. What's REALLY good in Hermes that Relix should have (the takeaways)

Prioritized, each mapped to where it lands in Relix. The top three are the ones that would most transform Relix.

### ⭐ A. The closed self-improving learning loop (the crown jewel)
Relix already has a skill store, memory, and confidence/judge (these were *inherited* from Hermes' lineage) — but the **loop is not closed**. Hermes closes it:
- **Autonomous skill creation from experience.** After a complex task (triggered by *tool-iteration intensity*, not turns — a proxy for difficulty), a background fork of the agent asks "should I save a skill?" and writes a class-level skill. The fork inherits the parent's exact cached system prompt, so it's ~26% cheaper than a fresh call.
- **The curator.** A background pass ages skills on a *usage-timestamp clock* (active→stale→archived at 30/90 days, with reactivation if used again), consolidates narrow siblings into umbrellas, and **never deletes — only archives**. Gated by `created_by:agent` provenance so it can only touch what the agent autonomously made (user-authored skills are off-limits).
- **The nudge.** Counter-based triggers fire the background memory/skill review *after* the response, so self-improvement never competes with the task.

**For Relix:** this is the biggest single thing to adopt. An agent (or the whole company) that learns from its work and curates that knowledge — with the provenance/never-delete safety invariants — is exactly the "self-organization / automatic organizational learning" Relix's own roadmap gestures at. It maps onto Relix's existing skill store + memory; what's net-new is the *loop* (creation trigger + curator + nudge + the provenance gate).

### ⭐ B. `execute_code` — RPC-from-script (zero-context-cost tool pipelines)
The model writes **one Python script** that calls tools over RPC; only its `stdout` returns to context, and the turn is *refunded* against the budget. An N-step deterministic pipeline (filter, loop, branch, reduce a huge output) collapses into one near-free inference turn — and the RPC routes back through the *same* tool-dispatch path, so all security/approval/audit still fire.

**For Relix:** enormously valuable for a multi-agent platform — it's the cheapest possible "deterministic glue" primitive, far cheaper than spawning an agent for mechanical work. It fits Relix's tool/SOL model directly (a script that calls `remote_call`-style tools over the local RPC, gated by the same admission pipeline). This is a near-free win.

### ⭐ C. Serverless execution backends (hibernate-to-$0, wake with exact state)
Six terminal backends behind one `BaseEnvironment` interface (local/docker/ssh/singularity/modal/daytona). The standout: **Modal snapshots the filesystem and recreates on demand; Daytona stops and resumes the same sandbox** — both keyed by `task_id`, both running `sync_back` on teardown. Between sessions *nothing runs* — only filesystem state persists — so a long-running agent task hibernates to ~$0 and wakes with its exact working state.

**For Relix:** this is the model for Relix's deferred "execution workspaces" phase. Instead of just a cwd or a git worktree, a `task_id → {filesystem snapshot | stoppable VM}` abstraction gives "agent runs for days, costs nothing when idle, wakes where it left off." Directly answers Relix's "Cloud / Sandbox agents" roadmap item, and the unified `execute()`-over-both-subprocess-and-cloud-SDK design is the clean way to do it.

### D. Memory operational rigor (security + cost)
Beyond Relix's existing four-layer memory, Hermes adds discipline worth copying:
- **Memory-context fencing as a *security* primitive** — recalled memory is wrapped in a trusted fence, forged fences are stripped from provider output, and a streaming state-machine scrubs fences across chunk boundaries (discarding unterminated spans). *Treat every recalled memory span as untrusted-by-default.* Relix's mesh, where agents may share memory across trust boundaries, needs exactly this.
- **The auxiliary-client router** — one cost/health-aware fallback chain for all background LLM work (compression, vision, embeddings, titles, session search), each provider declaring its own cheap model, with 402-failover. A platform running many agents wants exactly one such router.
- **LLM-free FTS5 session search with "bookends"** — cheap, deterministic cross-session recall (goal + match-window + resolution from one query, no LLM cost).

### E. Prompt-caching discipline
System prompt built once, replayed byte-stable, *never* mutated mid-conversation except on compression (which deliberately rotates the session). All ephemeral context goes into user/tool messages. This is a hard cost lever Relix's heartbeat loop should enforce per-agent.

### F. The cron cheap-precheck → conditional-wake pattern
Relix has cron/routines, but Hermes's is richer: a `no_agent` script job (zero LLM cost — a bash watchdog), a **wakeAgent gate** (the cheap script decides whether to spin up the expensive agent at all), `context_from` chaining (job A's output feeds job B), and at-most-once scheduling with period-scaled catch-up. The two-tier "cheap check first, only wake the model if needed" pattern is a big cost win for scheduled work.

### G. The trajectory training flywheel (model-level self-improvement)
Distinct from the skill-level loop: Hermes captures every run as a *training sample*, normalizes all reasoning into one `<think>` channel, synthesizes the canonical tool-schema preamble at save time, drops zero-reasoning samples, pads tool-stats to a stable schema, and **compresses long trajectories with the target model's own tokenizer** (protecting head+tail, summarizing the middle). Relix already has a training pipeline (Hermes lineage); the *normalization + schema-stability + tokenizer-aware compression* discipline is the part to nail.

### H. ProviderProfile + api_mode transports
A declarative `ProviderProfile` (all provider facts as data, user-overridable via a plugin dir) + `api_mode`→transport (wire-protocol isolated to one class with 4 methods). New model = a ~15-line profile; new protocol = one transport. Relix's AI node providers could adopt this exact shape.

### I. Kanban's operational resilience (fold into the heartbeat loop)
Hermes's kanban dispatcher has battle-tested mechanics Relix's assignment/checkout loop should copy: **lease + heartbeat with extend-if-alive / reclaim-if-wedged** (PID alive *and* activity-heartbeat fresh → extend the lease; wedged → reclaim), a **unified failure circuit-breaker with fast-trip classes** (clean-exit-without-completing = protocol violation → trip immediately; systemic error fingerprints → trip), **worker capability narrowing + ownership enforcement** (a worker may only mutate its own task), and the **activity→heartbeat bridge** (liveness as a side effect of normal token/tool traffic, no explicit heartbeat call). These are more robust than naive retry and map onto Relix's two-pointer checkout + conservative recovery.

---

## 4. What's BETTER in Paperclip than Hermes

Hermes is a phenomenal *single* agent but a weak *company*. Paperclip wins decisively on:

1. **The multi-agent company model.** Hermes has **no org chart, no roles/hierarchy, no capability-based routing, no goal hierarchy.** Its kanban routes by a flat assignee *string* and a single global default. Paperclip's `reports_to` org tree, chain-of-command, manager-subtree authority, Goal→Project→Issue spine, and goal ancestry are vastly richer for coordinating many agents toward an outcome.
2. **Governance.** Hermes's "governance" is per-task limits (a failure counter, ownership checks, dangerous-command approval). Paperclip has real governance: **approval gates** (hire / strategy / budget-override / high-risk), a **permission system** (the agent gate + scoped grants), **budgets with enforcement** (soft/hard, auto-pause, cancel work), and **Board oversight**. A company of agents needs this; Hermes lacks it.
3. **The work-object spine + goal-facing dashboard.** Paperclip makes "run a company" legible — issues as conversations, the board, the org chart, the Inbox. Hermes is a chat/TUI for one agent; its kanban dashboard is a side feature.
4. **True multi-company multi-tenancy with data isolation.** Hermes is single-user (profiles = isolated *instances*; kanban tenant = a soft column). Paperclip runs many companies in one deployment with hard isolation.
5. **The durable, outlive-the-turn agent model.** Hermes *deliberately* makes delegation **non-durable** — subagents are synchronous and cancelled on interrupt, their work discarded. Paperclip's entire value is *durable* agents working issues across heartbeats for days. For "assign it and it works autonomously," Paperclip's model is the right one — and it's exactly what the Relix execution design adopts.

---

## 5. What's completely different (the layers, restated)

They're not really competing — they answer different questions:
- **Hermes:** "How do I make *one* agent as capable and self-improving as possible?" → depth, learning, serverless, execute_code.
- **Paperclip:** "How do I run a *company* of agents toward goals, governed?" → org, issues, approvals, budgets.
- **Relix:** "How do agents run *securely* on a mesh?" → identity, policy, audit — *and now* also the company question.

Their security models are three different answers to three different threat models (protect one user / govern a company / secure a mesh). Their "learning" stories differ completely (Hermes self-improves; Paperclip doesn't learn at all; Relix has the primitives idle). You can't pick "the best one" — you compose them.

---

## 6. The synthesis: Relix can be all three

Relix is uniquely positioned because it already has the *substrate* (the secure mesh) and is *adopting* the company model (Paperclip). The missing third pillar — making the agents themselves **self-improving** — is Hermes's gift. The end state:

> **A secure mesh (Relix) running a governed company (Paperclip-model) staffed by self-improving, serverless-capable, cheap-to-run employees (Hermes ideas).**

No single existing system is all three. That's the differentiated thing Relix can be.

---

## 7. Concrete additions to the Relix design (mapped to the phases)

Folding the Hermes takeaways into the existing roadmap, without disrupting it:

- **Into Phase 3 (the heartbeat/assignment loop):** adopt kanban's **lease+heartbeat (extend-if-alive/reclaim-if-wedged)**, the **unified failure circuit-breaker with fast-trip classes**, the **activity→heartbeat bridge**, and **prompt-caching discipline** per run. These sharpen the loop's resilience and cost.
- **Into Phase 3/4 (execution):** add **`execute_code` (RPC-from-script, budget-refunded)** as a first-class cheap-glue tool, gated by the admission pipeline. High value, low cost.
- **New cross-cutting pillar — "Self-improvement" (a Phase 7+):** the **closed learning loop** — autonomous skill creation (background-review fork on tool-iteration intensity), the **curator** (usage-clock aging, umbrella consolidation, never-delete, `created_by:agent` provenance gate), and the **memory nudge**. Plus **memory-context fencing** as a security primitive and the **auxiliary-client router**. This is the org-learning Relix's roadmap already wants.
- **Into the workspace phase (deferred):** the **serverless `task_id → {snapshot | stoppable VM}`** model (Modal/Daytona-style hibernate/wake) instead of only cwd/worktree — answers "Cloud/Sandbox agents."
- **Into routines:** the **cheap-precheck → conditional-wake** cron pattern (`no_agent` script + wakeAgent gate + `context_from` chaining).
- **Into the AI node:** the **ProviderProfile + api_mode-transport** shape for clean "any model plugs in."
- **Into the training pipeline:** the **trajectory normalization + schema-stability + tokenizer-aware compression** discipline.

---

## 8. What NOT to take (where Hermes's assumptions don't fit Relix)

- **The single-host, SQLite, PID-liveness kanban transport** — the *semantics* (lease + heartbeat + breaker + ownership) transfer; the *transport* (one SQLite file, `start_new_session` subprocesses, host-local crash detection) does not. Relix's distributed signed mesh needs a real distributed claim store, which it already has the pieces for (the coordinator + admission pipeline).
- **Non-durable delegation as the coordination model** — Hermes's synchronous, interrupt-cancelled delegation is right for *bounded parallel fan-out within one turn*, but it is explicitly the *wrong* model for outlive-the-turn work. Relix's company model (durable issues + the heartbeat loop) is correct here; use Hermes-style delegation only for the bounded-fan-out case (and `execute_code` for mechanical glue).
- **"Smart model routing" as a feature** — Hermes doesn't actually have an internal complexity classifier (it delegates to OpenRouter's router + per-branch model overrides + the aux-cost chain). Relix *does* have a real complexity/tier router already — keep Relix's, don't downgrade to Hermes's plumbing-only version.
- **Single-user assumptions** (profiles-as-instances, soft-namespace tenancy, one-user memory model) — Relix is multi-tenant/multi-company; treat Hermes's per-user patterns as inspiration for the *per-agent* and *per-company* equivalents, not literal copies.

---

*Net: keep Relix's secure mesh, keep adopting Paperclip's company model, and add Hermes's self-improvement loop + execute_code + serverless backends as the three highest-value transplants. That combination is something none of the three reference systems is on its own.*

---

## 9. The integration decision — embed, don't rebuild

This comparison settled *what* to take. The follow-on decision — *how* — has its own design doc: [`relix-hermes-integration.md`](relix-hermes-integration.md).

The short version: rather than reimplement Hermes's worker capabilities in Rust, **Relix embeds an installed Hermes as the execution brain of each agent**, connects to it through a clean swappable seam (Hermes's own structured-runs API + MCP), and ships a first-class **bridge plugin into Hermes** so the agent reports up to Relix, asks Relix for approvals, and routes every boundary-crossing tool back through Relix's gate. Three dispositions for each capability: **Embed** (run the actual Hermes runtime — the worker brain, the learning loop, execute_code, the backends), **Transplant** (reimplement natively — only where it's the spine), and **Pattern** (build native but designed from Hermes's test-locked invariants — the work-kernel CAS rules, the session-concurrency fix, credential hygiene). The spine, governance, security, and the durable agent model stay **native Relix and never Hermes** — they're our identity, and the sandbox each Hermes runs inside is the OS-level boundary Hermes's own security model assumes but doesn't provide. See the integration doc for the full architecture, the security reconciliation, the complete fold-in catalog, the phasing (H0–H4), and the open decisions (licensing is the gating one).
