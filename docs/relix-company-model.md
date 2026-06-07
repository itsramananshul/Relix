# Relix Company Model — Design

> **Status:** Design / idea layer. This document describes *what we are building and why*, not *how the code is written*. Implementation details (schemas, function names, routes, file layout) are deliberately omitted because they will change as we build. If something here ever contradicts the code, the **idea** in this document is the source of truth for intent — the code should be reconciled to it, not the other way around. This supersedes and expands `docs/product-spine-roadmap.md`.
>
> **One-line goal:** turn Relix from a *control panel of capabilities* into a *company of AI employees you govern* — keeping Relix's signed-mesh execution substrate completely intact underneath.
>
> **Grounding:** the Paperclip references in this document are not from skimming. They are based on a complete, file-by-file read of the Paperclip reference codebase (the 86-table data model, all server routes and services including the ~10K-line execution engine, the full authorization model, all 162 design/skill docs, and the entire React dashboard). Where this doc says "Paperclip does X" or "X is net-new for us," it has been verified at the source level.
>
> **Companion deep-designs (all ideas-only):**
> - [`relix-execution-and-issue-design.md`](relix-execution-and-issue-design.md) — the Task → Issue object and the heartbeat/assignment + supervisory loop (exact lock/checkout/coalesce/recovery mechanics). Read alongside Phases 1 and 3.
> - [`relix-dashboard-design.md`](relix-dashboard-design.md) — the operator console reshape (shell, nav, the issue board, issue-as-chat-thread, org/permission panel, realtime) and the **chat companion** design. Read alongside Phases 4 and 6.
> - [`relix-agent-adapters.md`](relix-agent-adapters.md) — **the universal "plug in any agent" system: every agent is backed by a swappable adapter (Hermes, Claude Code CLI on your Max subscription, Codex CLI on your ChatGPT subscription, ACP, remote API, …). An agent record gains an adapter choice; assignment is unchanged.** Read alongside Phases 2–4 (its track is A0–A4).
> - [`relix-hermes-integration.md`](relix-hermes-integration.md) — **the deepest adapter, in detail: embed an installed Hermes as the agent's brain, plug Relix into it via a bridge plugin, and govern everything that crosses the sandbox wall.** Folds every Hermes takeaway into this plan. Read alongside Phases 3–6 (its track is H0–H4).

---

## 0. How to read this document

- **Sections 1–8** describe the target product: the mental model, the work objects, the org, permissions, execution, chat, and the dashboard.
- **Section 9** is the honest mapping onto what Relix already has (reuse vs. net-new), so we never reinvent something we already own.
- **Section 10** names where we deliberately diverge from Paperclip.
- **Section 11** is the incremental, room-by-room roadmap.
- **Section 12** lists the decisions we are leaving open on purpose.
- **Section 13** is the glossary — every term defined once.

Everywhere this doc says "Paperclip does X," it means the reference implementation in `references/paperclip`, which we studied at the code level. We borrow Paperclip's *product shape*; we keep Relix's *substrate*.

---

## 1. The core shift

**Today, Relix is machine-facing.** You hand-edit TOML, pick a node type, write SOL, and work starts by chatting — a "task" gets logged as a side effect. The dashboard is organized by *feature*: Memory, Skills, Confidence, Training, Reasoning, Policy, MCP… 22 panels. Everything works, but it feels like a pile of powerful tools with no spine. You are operating a machine.

**We want Relix to be goal-facing.** You state an outcome, hand it to an agent, and the system organizes the work — the way a task manager looks simple on the surface while org charts, budgets, and governance live underneath. The dashboard is organized by *work object*: Inbox, Issues, Projects, Goals, Org Chart, Agents, Approvals, Costs. You are running a company.

The shift is not a rewrite. Relix already has the expensive, hard-won part — the signed mesh, the admission pipeline, memory, tools, the durable ledger, approvals, budgets, audit. What's missing is a **product layer on top** that gives those powers a coherent organizing model, plus a **front-end reshape** so the dashboard hangs off that model. That's what this document specifies.

---

## 2. The mental model

Relix becomes a **company**:

- You are the **Board** — the human owner. You set direction, approve big moves, set budgets, and can pause or override anything at any time. Nothing important happens without your sign-off unless you explicitly delegate that authority.
- Agents are **employees**. Each has an identity, a job (role, title, department), a boss it reports to, a set of permissions (what it is allowed to do), a budget, and a lifecycle (hired → working → paused → terminated).
- The **CEO** is the apex employee. You give the CEO a goal; the CEO proposes how to achieve it, and — within the powers you grant it — assembles and runs a team to do it.
- Work is organized as **Goals → Projects → Issues**. A Goal is the "why." A Project is a workstream. An **Issue** is the atom: a single ticket assigned to a single agent, where the work *and* the conversation about it live together.
- A **Run** is one episode of an agent actually working on an issue (on the mesh). Issues accumulate runs over their life.
- **Governance** — approvals, budgets, the audit trail — wraps all of it.

The feeling we are copying from Paperclip: *you can look at Relix and understand your whole operation at a glance — who's doing what, what it costs, and whether it's working* — while the heavy machinery (the mesh, policy, audit) stays hidden until you want it.

---

## 3. The work-object spine

This is the backbone. Every screen, permission, and cost will hang off one of these objects.

```
Company  →  Goal/Initiative  →  Project  →  Issue  →  Run  →  Event / Approval / Budget
 (you)        "the why"          "a            the atom    one         the governance
                                  workstream"   of work     working     & money trail
                                                + the       episode
                                                conversation
```

Each object below is described by **what it is**, **what it holds** (conceptually, not as a schema), **its lifecycle**, and **how it relates** to the others.

### 3.1 Company

- **What it is:** the top-level container — your organization. One Relix instance can host more than one company, fully isolated from each other (Relix already enforces tenant isolation; a Company is the product-facing name for a tenant).
- **Holds:** a name and branding; a monthly budget; org-wide governance defaults (e.g. "do new hires need my approval?"); the set of agents, goals, projects, and issues that belong to it.
- **Lifecycle:** active → paused (everything stops) → archived.
- **Relates to:** owns everything else. Every other object belongs to exactly one company.

### 3.2 Goal (Initiative)

- **What it is:** the durable "why" — a high-level outcome you care about ("Ship the v1 product," "Reach 1,000 users," "Keep support response time under an hour").
- **Holds:** a title and description; an owner (usually the CEO or a senior agent); a status; optionally a parent goal (goals can nest into a hierarchy).
- **Lifecycle:** planned → active → achieved (or cancelled).
- **Relates to:** sits under the Company; Projects and Issues link up to a Goal so every piece of work can trace to a reason. This is **goal ancestry**: an agent working an issue can always see the goal it serves, not just the task title.

### 3.3 Project

- **What it is:** a workstream — a grouping of related issues under a goal ("Q3 marketing," "Auth rewrite," "Customer onboarding").
- **Holds:** a title; an optional link to a goal; a lead agent; a status; optionally a shared workspace/environment the project's work runs in.
- **Lifecycle:** backlog → planned → in progress → completed (or cancelled).
- **Relates to:** belongs to a Company, optionally links to a Goal, contains Issues.

### 3.4 Issue — the atom (the most important object)

The Issue is where everything converges. It is simultaneously **the unit of work** and **the conversation about that work**. There is no separate "chat with agent #3" window — you talk to an agent *on its issue*, in the issue's thread.

- **What it is:** a single ticket — "Write the landing page copy," "Investigate the failing test," "Plan the migration."
- **Holds (conceptually):**
  - a title and description;
  - **one assignee** — exactly one agent (or, when handed to a person, one human). Single-assignee is a deliberate invariant: it makes "who owns this right now" unambiguous and prevents two agents from clobbering each other.
  - a **status** you can drag across a board (e.g. Backlog → Todo → In Progress → In Review → Done; plus Blocked and Cancelled);
  - a **priority**;
  - links up the spine: a parent issue (for sub-issues), a project, a goal;
  - a **comment thread** — the conversation. You comment; the agent comments back with progress, questions, and results. System notes (status changes, run summaries) also land here.
  - **sub-issues** — child issues an agent (often a manager) creates to break the work down and assign to others;
  - **documents** — durable artifacts attached to the issue (a `plan`, a `design`, notes, deliverables);
  - **blockers** — first-class dependencies on other issues ("this can't start until that is done");
  - its **run history** — every working episode and its transcript;
  - governance context — its cost so far, any approvals it triggered, and a **billing code** so its cost can be attributed to the team that requested it.
- **Lifecycle (the board columns):** Backlog → Todo → In Progress → In Review → Done; Blocked is a side state; Cancelled is terminal. Only certain transitions are valid; an invalid drag is rejected with a clear message.
- **Relates to:** belongs to a Company; optionally to a Project and/or Goal; optionally to a parent Issue (sub-issue tree); assigned to one Agent; produces Runs; may block or be blocked by other Issues.

The Issue is what you create when you say "let's do this thing," and what an agent checks out to start working. Its thread is the single place the whole story of that piece of work lives.

### 3.5 Run

- **What it is:** one episode of an agent actually working — it wakes, does work on the mesh, and stops. An issue may have many runs over its life (one per heartbeat / continuation).
- **Holds:** which agent, which issue, when it started/ended, its outcome (succeeded/failed/etc.), the **transcript** (the tool calls, thinking, and output, rendered the way Relix already renders run transcripts), and the cost/tokens it consumed.
- **Lifecycle:** queued → running → succeeded / failed / cancelled / timed-out.
- **Relates to:** belongs to an Agent and an Issue; emits cost events and audit records (Relix already produces all of this — runs map onto Relix's existing flow-run + coordinator-attempt + signed audit machinery).

### 3.6 Event, Approval, Budget (the governance trail)

- **Event / Activity:** the durable record of what happened — every status change, comment, run, approval, cost. Powers the Activity view and the Inbox. (Relix already has a hash-chained per-node audit log and an activity surface; this is the product face of it.)
- **Approval:** a gate where the Board (or a delegated approver) must say yes before something proceeds — hiring an agent, approving the CEO's strategy, overriding a budget, or running a high-risk action. (Relix already has a full approval gate with signed one-shot tokens and standing approvals.)
- **Budget:** spend limits at the company, agent, and (optionally) project level, with soft warnings and hard stops that pause work. (Relix already has a budget enforcer + cost tracking.)

---

## 4. The org model — the company of agents

### 4.1 Agent = employee

Every agent is an employee record. Conceptually it carries:

- **Identity** — who it is on the mesh (Relix's signed identity bundle), plus a human-friendly name and icon.
- **Job** — a role (CEO, planner, engineer, researcher, designer, etc.), a title, and optionally a department/team label.
- **A boss** — the single agent it **reports to** (`reports_to`). This one link turns the flat agent list into an **org tree**. The CEO reports to the Board (you); everyone else reports to another agent.
- **Permissions** — what it's allowed to do (Section 5).
- **Runtime/autonomy settings** — whether it self-runs on a schedule, whether it wakes when assigned work, and how many things it can do at once (Section 6.3).
- **A budget** — its own spend cap.
- **Lifecycle status** — see 4.4.

### 4.2 The CEO apex

The CEO is the most powerful employee by default. You give it a goal in plain language ("grow our newsletter to 5k subscribers," "build me a competitor-research pipeline"), and — within the powers you've granted — it:

1. proposes a **strategy** (which you can require it to get approved before it spends real effort),
2. **assembles a team** — it can create/hire the agents it needs (a planner, workers, a reviewer), set them up, and define how they work,
3. **delegates** work down the org and supervises it.

Crucially, **none of this is hard-coded.** You don't write "the planner does X." You *ask the CEO* to make a planner that does X, and the CEO sets it up. The shape of the company is something you converse into being, bounded by the permission toggles. The read confirmed this is exactly how Paperclip works: the CEO is an ordinary agent whose *markdown charter* says "you lead, you don't do the work yourself, delegate to your reports, and hire a new report when a needed role is missing" — there is no compiled-in "CEO logic." How that works mechanically is Section 4.5.

### 4.3 The org chart

A visual tree of the company: the CEO at the top, reports beneath, down to the workers. Each node shows the agent's status (idle / running / paused / error), role, and a live sense of what it's doing. This is both a map ("who works here") and a control surface ("click an agent to see or change its permissions, budget, and work").

Two structural ideas the org tree gives us:

- **Chain of command** — walking *up* the `reports_to` links from any agent gives its escalation path. When an agent is stuck, it escalates up its chain rather than dumping the problem on you.
- **Manager subtree** — walking *down* from a manager gives everyone it's responsible for. This is the scope unit for delegated authority: "this planner may only assign work to agents *under it*."

### 4.4 Agent lifecycle

- **Pending approval** — a freshly created agent that needs the Board's yes before it can do anything. It appears in the org chart but is inert: it cannot run, be assigned work, or hold keys. (This is the gate that makes "the CEO spawned a new hire" safe.)
- **Idle** — hired and approved, at rest, waiting for work.
- **Running** — actively working a run.
- **Paused** — temporarily stopped (by you, or automatically when it hits a budget hard-stop).
- **Terminated** — let go; kept for history but never runs again.

Who can do what to an agent is itself a governed thing (Section 5): the Board can always pause/resume/terminate; whether an *agent* (like the CEO) can hire, set up, or manage *other* agents is a permission you toggle.

### 4.5 How an agent is configured — instruction bundles, not code

This is the mechanism that makes "I just ask the CEO to build me a planner" real, and it's worth stating plainly because it's how the whole company stays soft and conversational instead of hard-coded.

An agent's *behavior* is defined by a small **instruction bundle** — markdown files attached to the agent. (In Paperclip this is the agent's `AGENTS.md` charter, plus a per-heartbeat checklist, a persona/voice file, and a tools note; the runtime injects this markdown into the agent's context every time it works.) The runtime contains no "what a planner does" logic — it just feeds the agent its own job description. So:

- A **CEO** is just an agent whose charter says "lead, don't do IC work, delegate to your reports, hire when a needed role is missing" — and which holds the spawn/assign permissions.
- A **planner** is an agent you (or the CEO) describe in plain language — "read the codebase, make a plan, break it into issues, assign workers, review their results, assign the next slice" — and that description *becomes* its instruction bundle.

So "the CEO builds me a planner that does X" means the CEO calls the hire flow with a drafted instruction bundle (the planner's job description), an adapter/model, a `reports_to`, and a set of permissions — and, if you allowed direct spawning, the new agent goes live. **Nothing about the planner's job is compiled in.** The permission toggles are the *guardrails*; the instruction bundle is the *job description*; the API surface is *what it can act on*. This is exactly why the chat companion (Section 7) can stand up a whole org by conversation — it's writing instruction bundles and flipping toggles, not changing code.

For Relix, this means an **agent record needs five editable things**: its instruction bundle (markdown job description), its permission toggles (Section 5), its runtime/autonomy settings (Section 6.3), its budget, and its `reports_to`. The CEO and the chat companion can author all five.

---

## 5. Permissions & governance (the heart of this design)

This is the part you care most about: a **clean, per-agent set of toggles** that decide what each employee is allowed to do, with the CEO as the most-powerful by default and the Board sovereign over all of it.

### 5.1 Philosophy

1. **Default-deny.** An agent can do nothing it hasn't been granted. Knowing another agent or node exists confers no power to use it. (This is already Relix's core security stance — the responding node enforces. The permission model is the *product face* of that stance.)
2. **The Board is sovereign.** You can always pause, resume, terminate, reassign, override, and re-budget anything — regardless of what you've delegated. Delegated authority never locks you out.
3. **Permissions narrow, they never widen past the floor.** A per-agent toggle can only *grant within* what the company's security policy already allows. You can give an agent less than the policy floor, never more. (This matches Relix's gate: the agent gate is "additive narrowing" on top of the policy engine.)
4. **Powers are explicit and legible.** Every toggle has a plain-language meaning and every denial has a reason you can read. No silent magic.

### 5.2 The per-agent permission surface (the toggles)

For each agent, in the dashboard, you can set:

**A. Org powers**
- **Can spawn/hire agents** — may this agent create other agents? (On for the CEO by default.)
  - Sub-setting: **how it spawns** — *directly* (the new hire goes live, subject to the company's approval default) vs. *must route through its boss/the Board* (the new hire waits for approval). This is the toggle you specifically asked for: "if I want the planner to just spawn itself, I can; or it must send the request up."
- **Can set up / configure other agents** — may it edit other agents' instructions, tools, budgets? (Typically scoped to its subtree.)
- **Can manage other agents' work** — may it reassign or override an in-progress issue owned by someone it manages?

**B. Work powers**
- **Can assign/delegate work** — may it hand issues to other agents? With a **scope**:
  - *Anyone in the company*, or
  - *Only agents under it* (its manager subtree), or
  - *Only specific agents* (an allowlist), or
  - *Only within specific projects*.
  - This scope is exactly the "planner can only assign to its own workers" idea.

**C. Capability powers**
- **Tools it may use** — a per-tool / per-tool-category set of toggles (web, filesystem, terminal, browser, MCP, etc.). (Relix already gates tools by category and risk; this surfaces that as switches.)
- **Secrets/credentials it may access** — which stored secrets this agent can have injected into its work.
- **Risk ceiling** — the maximum risk level of action it may take on its own (safe → low → medium → high → critical). Above the ceiling → blocked or sent to approval.
- **Actions that always require approval** — categories (e.g. "send email," "deploy to production," "spend money," "delete data") that, even if otherwise allowed, pause for a human/approver yes. (Relix already has approval-required categories + signed approval tokens + standing approvals; this surfaces them per agent.)

**D. Autonomy & budget**
- **Scheduled heartbeat** — does this agent wake itself on a timer to check its work (autonomous), or only when given work (reactive)? On/off + interval.
- **Wake when assigned** — does it spring to life the moment it's handed an issue or @-mentioned? (Usually on.)
- **Concurrency** — how many things it can work on at once.
- **Budget** — its monthly spend cap.

> **Note on density vs. Paperclip (verified at source level):** Paperclip's *entire* core permission vocabulary is **8 keys** (`agents:create`, `tasks:assign`, `tasks:assign_scope`, `tasks:manage_active_checkouts`, `environments:manage`, `users:invite`, `users:manage_permissions`, `joins:approve`), and its per-agent permission UI is literally **two toggles** — "Can create new agents" and "Can assign tasks." Everything richer (tools, secrets, concurrency, spawn-routing) it pushes into adapter/runtime config, secret bindings, the `reports_to` tree, a company-level flag, or plugins. We are deliberately making the per-agent panel **denser and first-class**, because Relix's underlying agent-gate already *natively* understands tool categories, risk levels, secret access, and scopes. So this is not us inventing new machinery — it's giving Relix's existing, richer gate a clean operator face. (See Section 10.)

### 5.3 Scoped powers (the subtree idea)

The most important "advanced" permission concept: a power can be **scoped**. "Can assign work" isn't just on/off — it can be bounded to a project, a list of target agents, or a manager's subtree. This is what lets you safely say "the planner may freely assign work, but only to the five workers under it, only within the migration project." Scoping is what makes broad delegation safe.

### 5.4 The Board (you) — sovereign powers

Always available, never gated:
- approve or reject hires and strategies;
- set/change any budget at any level;
- pause, resume, or terminate any agent;
- reassign, cancel, or override any issue;
- read the full activity/audit trail.

The Board's home is the **Inbox** (Section 8.2): the one place that surfaces what actually needs you — pending approvals, budget alerts, blocked work, failures.

### 5.5 Approval gates

Some moves pause for a yes. The gate types:
- **Hire approval** — a new agent waits in "pending approval" until the Board (or a delegated approver) says yes; a pending agent appears in the org chart but is inert (can't run, be assigned, or hold keys). In Paperclip this is a single **company-wide** switch that **defaults OFF** (frictionless hiring). In our design we keep that company default *and* add the per-agent "how it spawns" setting (spawn directly vs. route the hire up) — the per-agent control is net-new.
- **Strategy approval** — when you hand the CEO a goal, you can require the CEO to present its plan and get your approval before it spends effort or builds a team. In Paperclip this is **only a prompt convention** (the approval type exists in the enum and renders in the UI, but no server code enforces it — it rides on the CEO's charter + a `request_confirmation` interaction). We make it a **first-class, enforced, queryable gate** — see Section 10. This is net-new.
- **Budget override** — when an agent hits a hard budget stop, work pauses and an approval is raised: raise the budget and resume, resume once, or keep paused.
- **High-risk action approval** — an action above an agent's risk ceiling or in an approval-required category pauses for a yes before it runs. (Relix already mints signed, one-shot approval tokens for exactly this, with standing approvals for "yes, for the next hour / 10 calls / $5.")

The throughline: **approvals are uncircumventable** — there is no code path that lets a "requires approval" action proceed without a valid approval. (This is already true in Relix's gate; we keep it.) The opt-in **autonomous Prime standing-authority** layer does not weaken this: it can take an approval action (approve a proposal, activate a planning hire, greenlight a planning Clearance) **only** by consuming a bounded **standing approval** the Board explicitly granted to the synthetic `__relix_autonomous_prime__` authority for that Guild — i.e. it operates *through* the grant, not around the gate. With no such grant every gate stays human, exactly as before. (Implementation: `docs/product-spine-implementation.md` "Prime standing authority"; gaps: `docs/current-limitations.md`.)

---

## 6. The execution model — how work actually gets done

### 6.1 The heartbeat / assignment loop

This is what makes "assign it and it works" real. When an issue is created/assigned (or an agent is woken):

1. **Wake** — the assigned agent is woken (because it was assigned, @-mentioned, its timer fired, or a dependency cleared).
2. **Check out** — the agent atomically **checks out** the issue, taking exclusive ownership of execution. If someone else already owns it, the agent backs off (it does not fight for it). This single-owner checkout is what prevents two agents double-working one issue.
3. **Work** — the agent does the work *through the mesh*: it calls the AI node, uses tools, reads/writes memory, and (if it's a manager) delegates to its reports. All of this runs through Relix's existing signed-and-audited admission pipeline — the product layer doesn't bypass any security.
4. **Communicate** — it comments progress on the issue's thread, attaches documents/results, and updates the status.
5. **Exit** — the run ends. The agent is not a long-running process holding state; it wakes, works, and stops. Its context for next time is preserved (Relix already persists per-task session state so an agent resumes where it left off).

### 6.2 Atomic checkout — no double-work

Exactly one agent owns an issue's active execution at a time. Checkout is the lock. A second wake on an already-owned issue is **deferred** (held and promoted later) rather than run concurrently. (Relix's coordinator already has single-active-execution semantics on its work items; we make this an issue-level product guarantee.)

### 6.3 Simultaneity — a whole team at once

Three layers, exactly as you pictured a planner running five workers:

- **Many agents run in parallel by default.** Each agent is independent — its own wakes, its own runs. A CEO, a planner, and five workers all work at the same time.
- **Per-agent concurrency** — one agent can work several issues at once up to its concurrency setting (or be forced to one-at-a-time).
- **One run per issue** — a single issue is never executed twice simultaneously. Parallelism comes from *having multiple issues*, not from racing one.

### 6.4 The orchestrator / manager pattern (your planner example)

This is the loop you described — "the planner reads the problem, makes a plan, spawns workers, they report back, it assigns the next piece." Here's how it works in this model, drawn directly from how Paperclip does it (event-driven, never busy-polling):

1. **Plan.** The planner reads the problem (codebase, context, memory) and writes a **plan document** on its issue. If you required it, the plan goes through strategy approval first.
2. **Decompose into sub-issues.** On acceptance, the plan becomes **child issues** — one per piece of work — each assigned to the right worker. This decomposition is **exactly-once**: even if the planner's run crashes and retries, the children are created once, never duplicated. (Paperclip fingerprints the accepted-plan revision and resumes partial work; we adopt the same guarantee.)
3. **Run in parallel.** Independent child issues (no blockers) start at once across the workers; dependent ones wait, marked **blocked**, until their prerequisite is done.
4. **The planner exits.** It does **not** sit and poll. It goes to sleep.
5. **It's woken when work lands.** Two automatic wake reasons drive the supervisory loop:
   - **children-completed** — when *all* of an issue's sub-issues finish, the parent's owner (the planner) is woken, with a digest of what each child produced.
   - **blockers-resolved** — when an issue's prerequisite finishes, the now-unblocked issue's owner is woken.
6. **Review & assign the next slice.** On waking, the planner reads the results, decides what's next, and creates/assigns the next batch of child issues — or marks the goal done. Loop back to 3.

This is the key to "one worker finished → the planner sees it and gives it the next task," without any agent burning budget in a polling loop. The org tree, sub-issues, blockers, and the two wake reasons together *are* the orchestration engine.

### 6.5 Blockers & dependencies

Dependencies are **first-class**: an issue can declare it is blocked by other issues. A blocked issue does not start until every blocker is *done* (a *cancelled* blocker does not count as resolved — that would be unsafe). This is what lets a manager express a real dependency graph and have independent branches run in parallel while dependent branches wait and auto-wake.

### 6.6 Cost rollup & attribution

When a manager delegates, the subordinate's costs **roll up** to the requester. Two mechanisms:
- **The work tree** — because sub-issues hang under their parent, the cost of all descendant work aggregates into the parent issue's subtree total. The planner's issue shows the cost of the whole effort it spawned.
- **Billing code & request depth** — work handed across teams carries a billing code so its cost attributes to the requesting team, and a "delegation depth" counter shows how many hops deep a cascade went. (Relix already tracks cost per agent/issue/run; we add the tree-rollup and the cross-team tag.)

> **Implementation note — the Allowance/budget window (SHIPPED).** The autonomous
> per-Operative **Allowance** hard-stop and the additive **Guild** budget
> hard-stop bill against a single canonical window: the **current calendar month
> in UTC**. There is **one** window function
> (`heartbeat::allowance_window(now_ms)`) that every spend read derives from — the
> dispatch gate (`dispatch_budget_admits`) and the Action Center's live-spend
> seam (`MetricsSpendSource::current_month`) both call it, so the gate and the
> Desk can never disagree by computing the window two different ways.
> - **Boundaries.** The window opens at the month's first instant
>   (00:00:00.000 UTC, **inclusive** — matching the ledger's
>   `timestamp_ms >= since` sum) and month-to-date spend is summed up to *now*;
>   `resets_at_ms` is the first instant of the next month — the reset edge.
> - **Reset bookkeeping.** There is no stored counter to clear: spend is always
>   re-summed from the live month start, so a new month is a fresh window **by
>   construction** (the reset is implicit in the moving `start_ms`).
> - **UTC is deliberate and fixed.** The mesh carries no per-Guild billing
>   timezone, so a single stable zone keeps the gate, the feed, and tests in
>   agreement. If a per-Guild billing timezone is ever introduced, it changes
>   only that one function.
> - **Manual sovereignty unchanged.** Only the autonomous heartbeat path passes
>   through this gate; a manual `brief.run` / `prime.start` never does (the Board
>   is sovereign).
>
> **Implementation note — issue-tree cost rollup + billing attribution
> (BACKEND SHIPPED).** Both §6.6 mechanisms now have a backend foundation:
> - **The work tree.** `brief.cost_rollup` (→ `GET /v1/spine/briefs/:id/cost`)
>   computes the cost of a Brief **and its entire same-Guild Sub-brief tree** by
>   summing the durable `brief_runs` ledger (real run `cost_micros` — never UI
>   data), returning own vs. descendant totals, a tree run/Brief count, and a
>   per-billing-code breakdown. It is **tenant-safe by construction**: the
>   recursive descent follows `spawned` edges only into same-Guild Briefs, so a
>   stray cross-Guild edge (and its whole subtree) is excluded, and a cross-Guild
>   caller reads not-found.
> - **Billing code (Brief + object-level, BACKEND SHIPPED).** An additive
>   `billing_code` on a Brief (set via `brief.set <id>|billing_code|<code>`,
>   surfaced on the Brief detail) AND an additive `billing_code` on **Mandate,
>   Campaign, and Guild** (set via `mandate.update`/`campaign.update
>   <id>|billing_code|<code>` and `guild.set_billing_code <code>`, surfaced on
>   their reads). When a run **starts**, its effective code is **stamped** onto
>   the run row with the full precedence: the **Brief's own** code → the nearest
>   same-Guild **ancestor Sub-brief**'s code → the linked **Campaign** code →
>   the linked **Mandate** code → the **Guild**'s own code. The object-level
>   steps resolve through a tenant-safe `ObjectBillingResolver` (the spine store)
>   injected into the Brief ledger — so a Brief in one Guild can never inherit
>   another Guild's Campaign/Mandate/Guild code, even with a bad/cross-Guild
>   link. Attribution is durable and point-in-time (a later change to ANY object's
>   code never rewrites a past run's bill). Manual and autonomous runs are
>   attributed identically (the stamp is in the shared `prepare_claimed_run` seam).
> - **Window.** The rollup bills against the **same canonical
>   `heartbeat::allowance_window`** (current UTC calendar month) the dispatch
>   gate uses; a caller/test may override with explicit bounded since/until.
> - **Delegation-depth counter + guard (BACKEND SHIPPED).** The
>   runaway-recursion backstop that complements the rollup. A Brief's
>   **delegation depth** is the longest same-Guild `spawned` parent chain up to
>   a root (root `0`, its Sub-brief `1`, …); `link_subbrief` — the single choke
>   point for direct `brief.subbrief`, the `suggest_tasks` accept
>   materialization, AND Mandate orchestration — refuses a link whose child
>   would exceed the central cap `MAX_SUBBRIEF_DELEGATION_DEPTH = 1024` (the
>   doc-LOCKED "≥1024 runaway backstop, not a product limit"). The
>   `suggest_tasks` accept pre-checks up front (no partial child creation; card
>   stays open). Depth is computed over same-Guild edges only, so a cross-Guild
>   edge can never inflate/leak another Guild's depth; `brief.detail` now
>   surfaces `delegation_depth` + `max_delegation_depth` for read visibility.
> - **Still deferred (honest):** the **frontend** Costs surface remains unbuilt.
>   (Object-level Mandate/Campaign/Guild billing **codes** are now shipped — see
>   the Billing-code note above.)

---

## 7. The chat companion — the reasoning front door

You described this precisely: not a separate dumb chat window, but a **context-aware companion** that can see everything happening in the company, that you reason with, and that turns conversation into structured work on command.

### 7.1 What it is

- **Context-aware.** The companion can read the live state of your company — current issues, agents, what's running, recent activity, costs. When you ask "what's the planner stuck on?" it actually knows.
- **A thinking partner.** You talk through what you're trying to do — "here's what I'm considering" — and it reasons back, proposes options, points out tradeoffs.
- **A materializer.** When you like a direction, you say it in plain language — *"make this an issue," "put this in production," "assign this to the CTO," "have the CEO spin up a research team for this"* — and it **creates the real work objects**: issues, assignments, even instructing the CEO to build a team. Conversation lands as durable, governed work.

### 7.2 How it relates to issues

Chat is the **front door for reasoning**; issues are **where reasoning lands**. The chat is ephemeral exploration; the moment something is worth doing, it becomes an issue (durable, assigned, governed). This is the bridge between "I want to think with the model" and "everything is issue-first." Chat doesn't bypass governance — anything it creates (an issue, a hire request) goes through the same permission and approval gates as if you'd clicked the buttons yourself.

### 7.3 A useful side effect

Because the chat surface stays, Relix keeps working as an OpenAI-compatible endpoint for external clients — but its *primary* chat becomes this company-aware companion, not a generic chatbot.

---

## 8. The dashboard — the reshape

The dashboard stops being organized by *feature* and starts being organized by *work object*. The 22 feature panels don't disappear — they **move under the objects they belong to**.

### 8.1 Navigation (hung off work objects)

- **Inbox** — what needs *you* (see 8.2).
- **Issues** — the board (kanban) + list of all work. The "task manager" surface.
- **Projects** — workstreams.
- **Goals** — the why-tree.
- **Org Chart** — the company of agents.
- **Agents** — the employee list + each agent's detail/permissions.
- **Approvals** — pending and past gates.
- **Costs** — spend by company / agent / project / issue, with budgets.
- **Activity** — the audit/event stream.
- **Chat** — the reasoning companion.

### 8.2 The Inbox (the Board's home)

A single action center showing only what needs you, in priority order: **approvals** (hire, strategy, budget, high-risk — with inline approve/reject), **alerts** (agent errors, budget thresholds), and **stale/blocked work** (things stuck with nobody moving them). It's computed from live state, not a notification table.

> **Implementation status — the Action Center (SHIPPED, read-only).** This is
> realized as **`company.actions`** → **`GET /v1/spine/company/actions`**: one
> tenant-scoped, ordered, deduped, **READ-ONLY** feed of the operator's next
> actions, **computed from existing live state — no notification table** (true to
> the section above). It is surfaced on the dashboard Overview Command Center
> with a button on each item that links to the existing governed route to act —
> the feed itself approves/runs/applies **nothing**.
>
> - **Categories implemented:** `approval` (pending hire/spawn Clearances + a
>   Mandate's *proposed* strategy gate), `hire` (Operatives `pending` and inert
>   until approved), **`budget`** (allowance-backed: committed Allowance over/near
>   the Guild budget, and an active Operative hard-stopped by a `0` Allowance that
>   has work waiting), `ready_to_start` (assigned-to-active + unblocked +
>   unclaimed Briefs — surfaced above generic blocked work so the operator can
>   move things forward), `blocked` (missing-assignee + dependency-blocked
>   Briefs), `needs_review` (a completed Shift awaiting review → apply),
>   `failed_or_refused` (a failed/refused/interrupted Shift — now a
>   **recovery-decision card** that names the root cause and recommends the
>   existing fix: assign · configure Rig · raise Allowance · review runtime ·
>   inspect), and `stale` (stuck-too-long work, lowest priority). **Ordering**
>   puts approvals/hire blockers on top, then budget governance (a hard-stop
>   blocks all of an Operative's work; over-commitment is a Board concern), then
>   recovery before informational stale, ready before generic blocked; **dedupe**
>   collapses the same underlying object so the feed never spams.
> - **How it fits the company model:** it is the Board's (§5.4) sovereign home —
>   the one surface that says "here is everything the Founder/Prime flow needs
>   you to decide or unblock right now," spanning the hire gate (§5.5), the
>   strategy gate (§5.5), **budget oversight (§5.4 — committed Allowance vs the
>   Guild budget)**, the assignment/heartbeat loop (§6), and the review → apply
>   loop. Every action stays behind its existing gate; the Action Center only
>   *routes you to it*. The Overview card **refreshes** off the existing run-event
>   stream (debounced change-trigger) with a low-frequency poll fallback — no new
>   event bus.
> - **Deferred (honest):** budget alerts are **allowance-config-backed only** —
>   the authoritative **live month-to-date spend** the dispatch gate enforces
>   (`cost_since` / the `over_allowance` path) is not threaded into this read-only
>   feed, so it shows no "spent $X of $Y" figure (over-spend surfaces reactively
>   as the `over_allowance` recovery card); recovery cards map the durable refusal
>   taxonomy to a recommended action but there is still **no diagnosis layer and
>   no per-run failure-class/retry-budget** (no true retryable-vs-not); the finer
>   `blocked` sub-reasons (missing-adapter, failed-preflight) are not separate
>   reasons here; and the refresh is low-latency event-trigger + poll, not
>   hard-realtime push of every field (the feed is capped with an honest
>   `truncated` flag).

### 8.3 The Issue detail (where work lives)

The centerpiece. One issue, showing: the description (inline-editable), the **conversation thread** (you + agent comments + system notes + the live run transcript, rendered as a chat), the **properties** (status, priority, assignee, project, goal), **sub-issues** with their progress, **documents** (plan/design/deliverables), **blockers**, **run history**, and its **cost**. Interactive prompts from the agent ("should I proceed?", "which option?") render as answerable cards right in the thread.

### 8.4 The Org Chart + the per-agent permission panel

The org tree (Section 4.3) is also the way you govern. Click an agent → see and toggle its permissions (Section 5.2), its budget, its autonomy, and its current work. This is the clean, structured permission surface you asked for — every switch in one place, per employee.

### 8.5 Where the feature panels go

- **Memory** → shown on an agent's page (its memory) and on the company (shared knowledge).
- **Skills** → on the agent that has them.
- **Confidence / Reasoning / Judge / Belief** → on a run's detail ("how sure was it, how did it decide").
- **Credentials / Secrets** → under Settings + per-agent access toggles.
- **Policy / Tenants / PII / Audit** → under Settings / Activity (governance).
- **Plugins / MCP / Tools** → under Settings (capabilities) + per-agent tool toggles.
- **Training / Metrics / Observability** → under Costs/Activity or a System area.

Nothing is lost; everything gets a *home on the object it describes*, instead of a top-level tab.

### 8.6 The feel (principles we copy from Paperclip)

- **Goal-facing, not log-worshipping** — the default view is a human summary, not raw output. Raw logs are one click deeper.
- **Progressive disclosure** — summary → steps/artifacts → raw transcript.
- **Time-to-first-success under five minutes** — setup generates/validates/explains every required value.
- **No silent failures** — every failed run is visible.
- **Dense but scannable, keyboard-friendly, dark-first.**

### 8.7 Concrete structural lessons from Paperclip's dashboard (verified, worth reusing)

The full read of Paperclip's React app surfaced a few load-bearing structural ideas we should copy outright:

- **One list component is the spine.** Paperclip's `IssuesList` *is* the product — it owns list↔board toggle, grouping (by status/priority/assignee/project/parent), sub-issue nesting, density controls, and a **"workflow checklist" rendering** (numbered steps `1`, `1.1`…, with inline "blocked by X · step N" chips) that makes a tree of work read like a goal-facing plan. The kanban board itself is "dumb" and density-driven. We should build *one* such issue surface, not many.
- **The issue is a chat thread, on a real agent-runtime.** The conversation surface is built on an agent-chat runtime that merges human comments, agent messages, live run transcripts, and interaction cards (answerable "ask / confirm / suggest-tasks" prompts) into one stable thread. This is what makes "talk to the agent on its issue" feel native.
- **A three-zone shell** (collapsible left nav, full-width content, contextual right "properties" panel) with the nav grouped into **Work** (Issues, Routines, Goals) and **Company** (Org, Skills, Costs, Activity, Settings) — plus an **Inbox** as the operator's action center. That grouping is the goal-facing orientation we're after.
- **The org chart doubles as the governance surface** — click an agent to open its detail, where its (two, in Paperclip) permission toggles live. Our denser per-agent panel lands in the same place.
- **Realtime is one WebSocket per company → surgical cache updates**, with rate-limited toasts and direct cache hydration of the visible issue (no full refetch). Relix already has a per-company live-events socket; this is the pattern to put in front of it.

---

## 9. Mapping onto Relix's existing substrate (reuse vs. net-new)

The point of this section: **we are not rewriting Relix.** Most of this is a product/UX layer over machinery that already exists. Honest accounting:

| New product concept | What Relix already has | What's net-new |
|---|---|---|
| Company | Tenant isolation (per-tenant policy, audit, stores) | Product-facing Company object + branding/budget surface |
| Goal / Initiative | — | New first-class object |
| Project | — | New first-class object |
| Issue (+ thread) | Coordinator **Task ledger** (durable: attempts, events, todos, edges, status machine, delegation) | **Evolve Task → Issue**: add single-assignee, board status, comment thread, sub-issues, documents, goal/project links, first-class blockers |
| Run + transcript | Flow runs + coordinator attempts + signed audit + run transcript rendering | Reuse as-is; surface on the issue |
| Agent = employee | Agent profiles (role, title, department, team, created_by, risk ceiling, allow/deny categories, approval-required categories, authorized approvers) | Add **`reports_to`** (the org tree); product surface |
| Org chart / chain of command / subtree | Delegation (parent/child task edges, depth cap, delegation executor) | Org-tree object + manager-subtree authority + the chart UI |
| Permissions & the gate | **Five-phase agent gate** (status → surface → risk ceiling → deny → allow), categories, **approval tokens (signed, one-shot)**, **standing approvals**, per-method policy | The **operator toggle UI** + scoped assignment grants + the org-power toggles |
| Approvals (hire/strategy/budget/risk) | Approval gate, Ed25519 tokens, out-of-band delivery + escalation over channels | First-class **hire** and **strategy** gates wired to the org flow |
| Budgets & cost | Budget enforcer (per-caller caps), cost tracking, alert engine | Company/agent/project budget surface + **tree rollup** + billing code |
| Heartbeat / assignment loop | Wakeups exist as parts (delegation executor, cron, AI planner; channels do task.create + ai.chat) | Assemble the **assign → wake → checkout → work → comment → exit** loop + the children-completed / blockers-resolved wakes |
| Chat companion | OpenAI shim + context-aware AI node (it can already read memory/state) | Make chat **company-aware** + able to **materialize work objects** on command |
| Dashboard | 22 feature panels; already Paperclip-inspired nav + spine-status badges | Re-nav around work objects; demote panels to detail tabs |

**Untouched (the engine room):** the libp2p signed mesh, the admission pipeline (identity → policy → handler → audit), memory (four-layer + vectors), the tool node (jail, SSRF guard, terminal, browser, MCP), the credential vault, PII gate, and the hash-chained audit log. The company model rides *on top* of all of it.

**Biggest net-new pieces:** Goals/Projects as objects, the org tree (`reports_to`), the assignment/heartbeat loop, first-class blockers, the strategy gate, the chat-to-issue companion, and the dashboard reshape.

---

## 10. Deliberate differences from Paperclip

Where we knowingly diverge (each is a choice, not an accident):

1. **A denser, first-class per-agent permission panel.** Paperclip's *whole* core is 8 permission keys and exactly **two** per-agent UI toggles, with everything richer externalized to config/plugins. We bring tool/secret/risk/scope/autonomy toggles into the core dashboard — because Relix's agent-gate already understands those dimensions natively. This is the structured permission surface you want, and it's strictly *more* than Paperclip exposes.
2. **Per-agent spawn routing.** Paperclip's "must a hire be approved?" is a single **company-wide** switch that defaults **off**. We make it **per-agent** (this planner may spawn directly; that one must route hires up) layered on a company default. Net-new.
3. **A first-class CEO strategy gate.** Paperclip leaves "approve the CEO's strategy" as a *prompt convention only* — the approval type exists in the enum and UI but **no server code creates or enforces it**. We make it a real, enforced, queryable gate, so "the CEO may not build a team until I approve the plan" is *enforced*, not merely *suggested*. Net-new.
4. **Instruction-bundle-driven agents in the core UX.** Both systems define agent behavior by markdown instruction bundles rather than code (Section 4.5). We lean into this as the *primary* way the chat companion and CEO assemble a company — authoring job descriptions + flipping toggles, conversationally.
5. **The signed-mesh substrate stays underneath.** Paperclip is a single trusted server. Relix keeps its decentralized, responder-enforced, audited mesh — so the whole company runs on a security model Paperclip doesn't have. The product layer must never bypass the admission pipeline; everything the chat companion or a manager agent does still passes identity → policy → audit.

---

## 11. The incremental roadmap (room by room)

We renovate while living in the house. Each phase leaves Relix running and is useful on its own.

- **Phase 0 — Foundations.** Promote Tenant → **Company** as a product object (name, budget). Add **`reports_to`** to agents (the org-tree link). Small, unlocks everything.
- **Phase 1 — The spine objects.** Add **Goal** and **Project**. **Evolve Task → Issue** (single assignee, board status, comment thread, sub-issues, documents, goal/project links, first-class blockers). After this, you can create and assign real issues.
- **Phase 2 — Org & Board.** The **org chart**, the **per-agent permission panel** (the toggles), the **Inbox**, and wiring the existing approvals/budget to issues and agents. The hierarchy you love becomes real and governable.
- **Phase 3 — The heartbeat loop.** Assign → wake → atomic checkout → work → comment → status → exit, plus the **children-completed / blockers-resolved** supervisory wakes and **exactly-once plan decomposition**. This makes "assign it and it works" true, and makes the planner/orchestrator pattern work.
- **Phase 4 — Hiring & the CEO flow.** The **hire approval** + **strategy approval** gates, so the CEO can be handed a goal, get its plan approved, and (within its toggles) assemble and run a team.
- **Phase 5 — The chat companion.** Make chat company-aware and able to materialize issues/teams on command.
- **Phase 6 — Dashboard reshape.** Re-nav around work objects; move the 22 feature panels to detail tabs. The full Paperclip *feel*.

(Phases overlap; the visible transformation is biggest in 1, 2, and 6.)

---

## 12. Open questions (decide as we build)

These are intentionally unresolved; we'll settle each when its phase arrives:

1. **Task→Issue migration:** do existing coordinator Tasks become Issues in place, or do Issues start fresh and Tasks remain as the low-level run record beneath them? (Leaning: Issue is the product object; the existing ledger becomes its execution substrate.)
2. **Strategy gate strictness:** is strategy approval required by default, or opt-in per goal/CEO?
3. **Spawn-team-in-one-approval:** when the CEO wants to stand up five agents, is that five hire approvals or one batched "approve this team" gate?
4. **Permission presets vs. raw toggles:** do we ship role presets (CEO / manager / worker / read-only) that set sensible toggle bundles, with raw toggles underneath for power users? (Leaning: yes — presets + override.)
5. **How much the chat companion may do autonomously:** can it create issues directly, or does it always show you a preview ("I'll create these 3 issues — confirm")? (Leaning: preview-then-confirm for anything that spends money or hires.)
6. **Goal/Project depth:** how deep do goal hierarchies and project nesting go before it's over-modeled?
7. **Blocker semantics on cancel/fail:** exact rules for when a blocked issue gives up vs. waits.

---

## 12.5 Prime Intelligence + Start-to-Shift (closing the product loop)

The Prime Assistant (§4.2, §7.2) gives the operator a governed
*describe → plan → approve* flow. Two gaps kept it from feeling like a real
product, and this section is the contract for closing them. Neither gap is
closed by faking model output, and neither bypasses a governance gate.

### A. Prime Intelligence — the plan must reflect the request

**The gap.** The proposal generator was templated to the point that two
different requests produced the same plan shape ("build a dashboard" and
"build a billing system" both yielded `Engineer track / Designer track /
Integrate`). That is honest about *not* using an LLM, but it is not useful
intelligence.

**The contract.** `prime.propose` stays **deterministic and honest**
(`ai_used:false` + an `ai_status` string — never silently presented as model
output; no language model is synchronously callable from a coordinator
handler today), but the rule-based planner MUST be **request-aware**:

- **Read the request, not just keywords.** Extract the concrete deliverable
  / subject of the work and carry it into the Mandate title and into each
  Brief title, so the plan names *what* is being built, not just a role.
- **Intent shapes the breakdown.** The Brief sequence differs by intent:
  - `fix` → a *reproduce → fix → verify* chain (a QA/verify Brief depends on
    the fix), not parallel role tracks;
  - `research` → an *investigate → synthesize/write-up* chain;
  - `build` → role tracks (one per inferred role) + an *integrate & ship*
    Brief that depends on every track;
  - `generic` → a single work Brief.
- **Role inference stays evidence-based.** Roles are inferred from the
  request (existing `classify`) and matched to **active** Operatives; a
  missing role is a `pending` hire suggestion, never a fake active agent.
- **The seam stays clean.** The generator remains a single PURE function
  (`agent/prime.rs::generate_proposal`) so a future model can replace the
  *interpretation* step while reusing the identical governed `prime.approve`
  / `prime.start` execution path. Honesty is mandatory: AI-unavailable is
  stated, not hidden.

### B. Start-to-Shift — the operator can actually start the planned work

**The gap.** After `prime.approve` created the Mandate + Briefs + crew
assignments, the operator had to leave the Prime flow and start each Brief
by hand from the board. The "I described it and watched it run" moment never
arrived through Prime.

**The contract.** A new governed capability **`prime.start`** turns an
**approved** proposal into running **Shifts** — but it invents NO new
execution path. It funnels every Brief through the SAME run chokepoint the
manual `brief.run` and the autonomous heartbeat already use
(`preflight_run` → `prepare_claimed_run` → `execute_ready`):

- **Approved-only.** `prime.start` operates on a proposal whose status is
  `approved` (so its Mandate + Briefs already exist). A non-approved or
  unknown / cross-Guild proposal is refused (not-found, no existence leak).
- **Only the ready Briefs run.** A created Brief is started **only** when it
  is ready to work — assigned to an **active** Operative, unblocked, not
  already claimed/running, and not already complete. Every Brief that is NOT
  started is returned with an **honest reason** (unassigned / blocked /
  already complete / cancelled / not currently startable), so the operator
  can see exactly what still needs a Clearance or a dependency.
- **Completes greenlit assignments (reconciliation).** `prime.approve`
  assigns each track only to the Operatives that were **active then**, and
  files the missing roles as `pending` hires. When the operator later
  **greenlights** one of those hires (pending → active via
  `agent.approve_hire`), its planned role-track Brief is still unassigned — so
  without this it would skip as *unassigned* forever and any dependent Brief
  (the *integrate* track that depends on every track) would never unblock. So
  before it reads the ready set, `prime.start` **completes the assignment
  `prime.approve` already planned**: for any still-unassigned planned track
  whose role now has an active Operative, it assigns that Operative — using the
  **identical role match** `prime.approve` used. This is the operator's
  sovereign completion of the approved plan (the whole flow is
  operator-initiated): it **never clobbers an existing assignee**, and it
  assigns nothing the operator did not already greenlight as a hire for that
  role. The reconciled Briefs are returned in `assigned` and each gets a
  `prime.assigned` Chronicle event. This is what lets the loop *not stop at
  hire* — greenlight the hire, Start the work, and the freshly-staffed track
  runs in the same call.
- **Real Shifts, same gates.** Each started Brief goes through the existing
  pre-flight: the assignee's Rig is resolved and **probed** (an unavailable
  adapter refuses cleanly and records a durable refused Shift — never a
  faked run), the single-owner **Claim** is won, the durable `brief_runs`
  ledger row is opened (stamped `manual` — `prime.start` is operator-
  initiated), and the blocking adapter call is handed to a background thread.
  The response returns the `run_id`s so the dashboard can watch each Shift
  move `running → done/failed/continued` via `/v1/runs`.
- **Sovereign, operator-initiated.** Like `brief.run`, `prime.start` is a
  deliberate operator action and carries the same semantics as a manual run
  (the per-Operative Allowance hard-stop is enforced on the autonomous
  heartbeat path, not on operator-initiated runs — the Board is sovereign;
  the single-owner Claim still prevents double-work). It changes no budget,
  hires no one, and runs nothing that is not already an assigned, ready Brief.
- **Audited.** `prime.start` records an Orchestration run (`mode:"start"`)
  on the Mandate and a Chronicle event on each started Brief, so the
  *what Prime suggested → what was approved → what was actually run* trail is
  complete.
- **It is not autonomy.** `prime.start` still requires the operator to click
  start; it does not propose hires, approve hires, or loop on its own. The
  assignment reconciliation above is **not** autonomous staffing — it only
  completes assignments the operator already authorised by *approving the
  hire*; `prime.start` hires/approves/creates no one. It is the governed
  trigger that lets the heartbeat/assignment loop (§6.1) begin for a planned
  Mandate in one step instead of Brief-by-Brief.

**The closed loop:** describe in Chat → `prime.propose` (a request-aware
plan, nothing created) → **Approve & create** (`prime.approve` — Mandate +
Briefs + assignments + pending hires) → **greenlight the hires** (approve the
pending hire / any spawn Clearance — the missing-role Operatives go active) →
**Start the work** (`prime.start` — reconciles the now-active hires onto their
waiting tracks, then runs every ready Brief) → **review-to-done each Shift**
(accept its run, then `run.apply` — which advances the Brief to board `done`)
→ its dependents (e.g. the *integrate* track) unblock and run on a repeat
**Start the work**. Every step is a governed gate; nothing runs itself.
(A dependent Brief unblocks only when *every* blocking track reaches board
`done`. A finished Shift opens its *run* review but does **not** move the
Brief on its own — the **operator's review-to-done** does, and that
review-to-done is now a single governed action: the operator accepts the run
and applies it, and a clean accept-gated **`run.apply` advances the Brief from
`in_review` to `done`** (resolving its dependents' blockers) — so no separate
manual `brief.move done` is needed. Apply stays the file-integration step;
it advances the board only on a clean apply and only for a Brief genuinely
awaiting review. **By default that accept + apply is a human's** — but under the
two separate, default-OFF standing grants of **§12.5G (Prime Shift Disposition
v1)** the autonomous loop may accept and apply a completed Shift on the Board's
behalf, through these exact review/apply paths and safety, never combined into one
power.)

### C. Prime Deliberation v1 — a model may CHOOSE among governed actions (opt-in)

**The gap.** The opt-in autonomous Prime loop (§5.4 / §8.2) was a *hardcoded
deterministic state machine*: each tick computed the single legal next governed
action and took it. Useful and honest, but the loop itself never reasoned — the
only model seam in the whole Prime flow was the request-time `prime.propose`
draft (§A), never the autonomous loop's per-tick choice.

**The contract.** Behind an explicit, default-OFF switch
(`RELIX_PRIME_LLM_DELIBERATION`), the loop may consult a model to **choose among
the actions it has already computed** — but **the model is NOT the permission
system.** The security invariant is absolute and unchanged:

- **Compute first, then ask.** Each tick still computes the SINGLE legal next
  governed action for a candidate exactly as before. The model is offered ONLY
  `[<computed action>, none]` and is asked to **confirm** the computed action or
  **HOLD** (`none`) this tick. It can never invent an action, name an action
  outside the candidate's allowed set, or widen the menu.
- **Strict server-side validation.** The model's reply must be strict JSON
  `{"action":…,"reason":…}`; a strict validator
  (`prime_deliberation::parse_prime_decision`) rejects unknown / disallowed
  actions, malformed / array / scalar / over-long output, and unsafe
  (over-long / control-char) reasons. Any rejection degrades to the
  deterministic action.
- **Execution is unchanged and fully governed.** A confirm runs the EXACT SAME
  governed handler the deterministic loop runs — standing authority, budget
  hard-stop, Claim, adapter probe, and tenant isolation all still apply. A
  model can never approve a gate it lacks a standing grant for, nor bypass a
  budget/Claim/adapter check. A `none` skips with zero side effects.
- **Honest provenance.** Every tick record carries `ai_mode`
  (`deterministic_only` / `llm_used` / `fallback` / `unavailable`) + `ai_reason`,
  surfaced on `prime.autonomy_tick_now`, so the operator always sees how the
  action was chosen and degradation is never hidden as model output.
- **No keys in the coordinator.** The live decider performs ONLY the existing
  `ai.chat` mesh call to the AI peer (alias `RELIX_PRIME_AI_PEER` default `ai`;
  session `RELIX_PRIME_LLM_SESSION` default `prime-autonomy`), using the SAME
  `{session_id,prompt,history}` shape as the bridge — no provider key enters the
  coordinator, the web bridge config, or the dashboard; a missing mesh / AI peer
  reads `unavailable` and falls back deterministically.
- **The manual tick uses the same live path.** The operator **Run Prime now**
  wake-up (`prime.autonomy_tick_now`) builds the SAME `MeshAiDecider` from the
  coordinator's populated outbound mesh client as the background timer, so it
  exercises live deliberation whenever the mesh AI peer is reachable (the
  controller runs the tick from a blocking thread so the decider's
  `Handle::block_on` never runs on an async worker). Live deliberation depends on
  a **populated coordinator mesh client and a reachable AI peer**; without them
  the manual tick honestly reads `unavailable` and falls back deterministically.

**It is not freeform Prime.** This is *constrained deliberation over the existing
action menu* — confirm-or-hold one computed action with a short reason. It does
not author the *action choice* freely, invent a goal, pick which identity to hire,
or call tools. (The *body* of a proposed strategy may be model-authored under a
separate opt-in switch — see §D — but its **approval** remains deterministic /
governed.)

### D. Prime Strategy Authoring v1 — a model may author the PROPOSED strategy text (opt-in)

**The gap.** When the driver drafts a Mandate strategy (§A / §5.4), the body was
deterministic-only — a templated objective/constraints/tracks/execution doc. The
loop could *propose* a strategy but never *reason* about its content.

**The contract.** Behind an explicit, default-OFF switch
(`RELIX_PRIME_LLM_STRATEGY_DRAFT`), when the autonomous/manual-tick loop executes
`propose_strategy` and a live mesh decider is available, a model may author the
**body** of the proposed strategy — but **the model is NOT the permission system**,
exactly as in §C:

- **Body only, still PROPOSED.** The model authors the strategy *text* from a
  bounded, secret-free snapshot (Mandate title / status / bounded description /
  active work roles / Brief readiness counts — never secrets, tokens, repo/file
  content, or large dumps; the prompt is length-capped). The result is proposed
  through the EXISTING `mandate.strategy.propose` handler and lands `proposed`; the
  human `mandate.strategy.approve` gate is unchanged. **The model never approves or
  executes a strategy.**
- **Strict server-side validation + sanitization.** The reply is re-validated by
  `prime_strategy::validate_strategy_draft`: it rejects empty / over-long output
  and obvious prompt-injection boilerplate, sanitizes the pipe to `/` + control
  chars, appends a standard "DRAFT / not approved" governance footer when the model
  omits it, and bounds the final doc to `STRATEGY_DRAFT_BODY_CAP` (footer
  preserved). Any rejection degrades to the deterministic `draft_mandate_strategy`.
- **Never overwrites.** The classifier only yields `propose_strategy` for a Mandate
  with **no** strategy, so an existing `proposed` / `approved` / `rejected` strategy
  is never overwritten or re-authored — a human rejection stays final.
- **Honest provenance.** Each tick record carries `strategy_ai_mode`
  (`deterministic_only` / `llm_used` / `fallback` / `unavailable`) +
  `strategy_ai_reason`, distinct from the action-choice `ai_mode`, surfaced on
  `prime.autonomy_tick_now`.
- **No keys in the coordinator.** It reuses the SAME `ai.chat` mesh path + decider
  (AI peer `RELIX_PRIME_AI_PEER`, session `RELIX_PRIME_LLM_SESSION`) as §C — no
  provider key enters the coordinator, web bridge, or dashboard; an unavailable
  peer falls back deterministically.
- **Independent of §C.** The action choice (§C) and the strategy body author (§D)
  are separate switches: a Guild can run deterministic action selection with a
  model-authored body, or vice versa. If §C holds (`none`), no strategy is drafted.
- **Explicit click stays deterministic.** Model-backed authoring is wired into the
  autonomous loop and the manual **Run Prime now** tick only; the operator one-click
  `prime.advance {action:"propose_strategy"}` route remains deterministic by design.

### E. Prime Executive Prioritization v1 — a model may CHOOSE the candidate ORDER (opt-in)

**The gap.** Candidate discovery/order was fixed-deterministic: standing-approvable
proposals first, then approved proposals, then bare Mandates, in store order. With
`RELIX_AUTONOMOUS_PRIME_MAX=1` the loop spent its single tick action on the *first*
deterministic candidate even when another already-legal candidate was more important.

**The contract.** Behind an explicit, default-OFF switch
(`RELIX_PRIME_LLM_PRIORITIZATION`), when the autonomous/manual-tick loop has ≥2
candidates carrying a positive **attemptable** action and a live mesh decider is
available, a model may choose the **order** in which the bounded tick spends its
action budget — but **the model is NOT the permission system**, exactly as in §C/§D:

- **Reorder (or hold) only — never widen.** The loop first builds the SAME
  deterministic candidate queue as before (the fallback order) and classifies each
  candidate **read-only** into the one next governed action it would run today. Only
  candidates with a positive *attemptable* action are offered (an approval-category
  action — hire / clearance / strategy / proposal-approve — is attemptable only with
  the matching live standing grant + known Rig; a pure human gate / running / done
  candidate is recorded deterministically but never offered). The model may only
  reorder the offered candidate keys, or return an **empty** order to HOLD the whole
  queue this tick. It can never invent a candidate, add an action to the menu, change
  a candidate's action, approve a gate it lacks a standing grant for, or bypass any
  budget / Claim / adapter / tenant gate — every executed step still flows through the
  EXACT SAME governed handler + gates.
- **Strict server-side validation.** The reply is validated by
  `prime_priority::parse_priority_order` against the offered keys only: an unknown key,
  a duplicate, a non-array / missing `order`, more keys than offered, a non-string key,
  malformed/array/scalar/over-long JSON or prose, or an over-long / control-char reason
  all degrade to the deterministic discovery order. An empty order is honoured as a
  hold (zero side effects) only when the output is otherwise valid.
- **Bounded execution.** The validated order is executed until `RELIX_AUTONOMOUS_PRIME_MAX`
  actions are spent; remaining attemptable candidates record `skipped` with their rank,
  and a held queue records every offered candidate `skipped` with no side effects.
- **Honest provenance.** Each tick record carries `priority_ai_mode`
  (`deterministic_only` / `llm_used` / `fallback` / `unavailable`) + `priority_ai_reason`
  + this candidate's `priority_rank`, distinct from the action-choice `ai_mode` and the
  strategy-body `strategy_ai_mode`, surfaced on `prime.autonomy_tick_now`.
- **No keys in the coordinator + independent of §C/§D.** It reuses the SAME `ai.chat`
  mesh path + decider (AI peer `RELIX_PRIME_AI_PEER`, session `RELIX_PRIME_LLM_SESSION`)
  as §C/§D — no provider key enters the coordinator, web bridge, or dashboard; an
  unavailable peer falls back deterministically. The three switches are independent: a
  Guild may enable any combination. With this switch off (or <2 attemptable candidates)
  the discovery order is byte-for-byte the legacy behaviour.

### F. Prime Orchestration Authoring v1 — a model may AUTHOR the Brief text (opt-in)

**The gap.** `mandate.orchestrate` (§4.6) materialises an idempotent three-tier Brief
tree (parent → role tracks → per-agent subject executions, with placeholder tracks for
staffing gaps), but the work-object *text* was mechanical: a fixed role→title map and a
single generated parent dossier. The tree was correct and stable but the titles /
dossiers read as boilerplate, with no per-track checklist.

**The contract.** Behind an explicit, default-OFF switch
(`RELIX_PRIME_LLM_ORCHESTRATION`), when the autonomous/manual-tick loop advances
`orchestrate_assign_ready` and a live mesh decider is available, a model may **author the
text** — titles, dossiers, checklists — of the orchestration skeleton, but **the model is
NOT the permission system**, exactly as in §C/§D/§E:

- **Author text only — never the skeleton.** The deterministic readiness logic computes
  the entire skeleton (which roles get tracks, which active agents get subject Briefs,
  which gaps get placeholders) exactly as before. The model is offered ONLY the stable
  role keys (the active roles) + subject keys (their staffed agent ids) + the parent, and
  may author a title / dossier / checklist for each. It can never invent a role, agent,
  Brief id, source marker, dependency, assignee, approval, budget change, or tool; the
  roles, agents, assignments, reviewer stamping, `max_briefs` cap, placeholder behaviour,
  and source-marker idempotency are byte-for-byte the deterministic path.
- **Strict server-side validation.** The reply is validated by
  `prime_orchestration::parse_orchestration_blueprint` against the offered keys only: an
  unknown top-level / role / subject key, more entries than offered, an array where an
  object is expected, a non-string / over-long title / dossier / checklist item, too many
  checklist items, malformed/array/scalar/over-long JSON or prose all degrade to the
  deterministic titles + dossiers. Pipe is sanitized to `/`; control chars are stripped.
  The VALIDATED blueprint (never raw model output) is the only thing passed into the
  orchestration handler.
- **Applied to new items only; hand-edits preserved.** A blueprint title/dossier is
  applied ONLY to a NEWLY-created Brief (the handler reuses existing Briefs by source
  marker and sets a title only on creation), so a rerun never duplicates a Brief and a
  hand-edited (or previously model-authored) title is never clobbered. Placeholder/gap
  tracks keep their deterministic `… track blocked:` text so the placeholder→active title
  promotion still works.
- **Honest provenance.** Each orchestrate tick record carries `orchestration_ai_mode`
  (`deterministic_only` / `llm_used` / `fallback` / `unavailable`) +
  `orchestration_ai_reason`, distinct from the action-choice `ai_mode`, strategy-body
  `strategy_ai_mode`, and queue-order `priority_ai_mode`, surfaced on
  `prime.autonomy_tick_now` and the Settings tick table (`orch:`).
- **No keys in the coordinator + independent of §C/§D/§E.** It reuses the SAME `ai.chat`
  mesh path + decider as §C/§D/§E — no provider key enters the coordinator, web bridge, or
  dashboard; an unavailable peer falls back deterministically. The four switches are
  independent: a Guild may enable any combination. With this switch off the orchestration
  text is byte-for-byte the deterministic v1, and the **direct one-click**
  `mandate.orchestrate` / `prime.advance {action:"orchestrate_assign_ready"}` route stays
  deterministic (it never builds a blueprint).
- **Governed Dossier persistence (whichever path authored the text).** Whether the
  parent / role-track / subject-execution / placeholder plan text is the deterministic
  default or model-authored, the orchestration path now persists it through the
  **governed, append-only, lock-aware** Dossier-authoring path (`author_dossier`, via a
  single `TaskStore::author_prime_dossier` helper) rather than the legacy author-less
  `add_dossier`. The write is stamped with the synthetic autonomous-Prime authority
  `__relix_autonomous_prime__`, is **idempotent** (a rerun never appends a duplicate
  revision), **respects explicit Dossier locks** (a kind locked by a different subject is
  refused, never overwritten), and **never clobbers a human/editor (or legacy
  author-less) latest revision** — only the first, Prime-owned `create` revision of each
  stable kind (`orchestration` / `execution` / `blocker`, none renamed) is ever written.
  The per-doc outcome (`authored` / `already_present` / `locked_by_other` /
  `skipped_human_owned` / `stale`) is reported on the orchestration result's
  `dossier_notes`. This makes Prime's own generated plan text first-class governed
  document state, not an off-to-the-side raw insert — without granting any freeform
  agent document editing (an operator's Dossier is never agent-authored).

### F-bis. Prime Plan-Package Authoring v1 — a model may PROPOSE a Brief decomposition (opt-in)

**The gap.** The planner pattern's front door — the **plan package** (an immutable
`plan` Dossier revision + a linked `suggest_tasks` proposal + an approval-bound
`confirm`, opened atomically by `TaskStore::open_plan_package`, execution-and-issue
§1.7/§1.8/§3.1) — existed only as a **manual** surface (the dashboard composer / the
chat companion). The autonomous Prime loop had no way to *propose* a decomposition: it
could plan, staff, orchestrate, prioritize, start, and dispose, but a Brief that simply
needed breaking down sat idle. There was no autonomous LLM planner and no model-chosen
`create_document` + `create_interaction`.

**The contract.** Behind an explicit, default-OFF switch
(`RELIX_PRIME_LLM_PLAN_PACKAGE`), when the autonomous/manual-tick loop reaches a
candidate the existing governed flow leaves **idle**, Prime may OPEN a plan package on a
single un-decomposed Brief and **leave the confirm OPEN for a human** — but **the model
is NOT the permission system**, exactly as in §C/§D/§E/§F:

- **Propose only — the AUTHORING layer never approves, assigns, or creates children.**
  Prime opens the package through the EXISTING `open_plan_package` primitive (the
  interactions stamped with the synthetic `__relix_autonomous_prime__` authority) and stops.
  The authoring layer does **not** accept its own `confirm`; children are materialized only
  when an approval accepts through the EXISTING `brief.plan_confirm_respond` /
  `respond_plan_confirm` path and the EXISTING exactly-once decomposition ledger (§1.7).
  That approval is either a human, **or** — under the explicit, default-OFF
  `prime.plan_package.approve` standing grant (F-quater below, distinct from §G's review/
  apply) — the autonomous loop accepting its OWN Prime-authored package. Children always
  open **unassigned** (no model-chosen assignee).
- **The model authors content only.** It may write the plan title/body, the approval
  summary, and a bounded list of proposed child Briefs (title / priority / a backward
  `after` dependency) — nothing else. It can never choose a method, capability, or tool,
  assign an agent, mutate an existing Dossier, or approve anything.
- **Strict server-side validation.** The reply is validated by
  `prime_plan_package::validate_plan_package`: output is size-bounded, a code fence is
  stripped, every string is secret-redacted + length-bounded, children are capped at
  `MAX_AUTONOMOUS_CHILDREN` (8, tighter than the store's 20), invalid priorities are
  dropped, and each `after` is remapped to a strictly-earlier kept sibling (forward / self
  / unknown / dropped-target refs dropped) so the store's own `normalize_proposal` always
  accepts the result. Empty body / no usable child → reject → deterministic fallback.
- **Dedup-guarded + non-clobbering + tenant-scoped.** Prime acts ONLY when the Mandate
  has a SINGLE non-terminal, childless Brief with **no** `plan` Dossier, **no** `plan`
  lock, and **no** open plan package — so a human/Prime/stale plan or an existing open
  package is never overwritten or duplicated (it reports `already exists` / `already
  awaits approval` and authors nothing). All reads/writes are scoped to the candidate's
  own Guild; another Guild's Brief is invisible.
- **Deterministic fallback.** On disabled / no-decider / unavailable / malformed output
  the content degrades to a safe deterministic plan→build→verify decomposition (carrying
  the DRAFT/not-approved language), with an honest provenance mode.
- **Honest provenance.** The tick record carries `plan_package_ai_mode`
  (`deterministic_only` / `llm_used` / `fallback` / `unavailable`) +
  `plan_package_ai_reason` plus the opened `plan_doc_id` / `suggestion_id` /
  `confirm_id` / `child_count` (ids/counts only — the plan body is never put on a record
  or Chronicle event), surfaced on `prime.autonomy_tick_now`.
- **No keys in the coordinator + independent of §C/§D/§E/§F.** It reuses the SAME `ai.chat`
  mesh path + decider as the other authoring layers — no provider key enters the
  coordinator, web bridge, or dashboard; an unavailable peer falls back deterministically.
  The switches are independent. **Honest scope:** by default (the `tail` trigger) this is a
  deliberate gap-filler placed at the tick tail, so it fires only for a candidate the
  existing flow leaves idle (e.g. a Mandate whose lone Brief is `blocked`); the
  **`before_execute` trigger (F-ter, below)** makes it also pre-empt a raw start. It never
  decomposes multi-Brief / orchestrated Mandates or scans every leaf Brief.

### F-ter. Prime Active Planner Trigger v2 — plan BEFORE executing (opt-in)

**The gap.** F-bis only fired at the **idle tail**: a Brief that was ready to *start* would
just start raw and undecomposed; Prime never got the chance to propose a decomposition
*first*. That kept the planner conservative — Prime could fill gaps but could not actively
say "before we run this, here's how I'd break it down."

**The contract.** A second, *layered* switch `RELIX_PRIME_PLAN_PACKAGE_TRIGGER` selects WHEN
F-bis fires — it changes only the **timing**, never the contract above (still propose-only,
still server-validated, still confirm-left-open, still no self-approval / assignment / child
creation). It is **inert unless** the master `RELIX_PRIME_LLM_PLAN_PACKAGE` switch (F-bis)
is on.

- **`tail` / `gap_fill` / blank — the default.** F-bis's v1 behaviour exactly: author a plan
  package only at the idle tail; never pre-empt a start.
- **`before_execute` / `plan_before_execute` — the active planner.** Before starting a lone
  **eligible** un-decomposed leaf Brief that would otherwise be started, Prime opens the
  proposed plan package **first** and **holds** the raw start, leaving the confirm OPEN for a
  human. The idle-tail gap-fill still runs as the catch-all. It pre-empts **only** a lone
  eligible leaf start: it never interrupts a higher-priority governance gate (proposal /
  strategy approval, team plan, hire / Clearance — those are different phases), and it never
  touches a multi-Brief / orchestrated Mandate. While a package is **pending approval** the
  start stays held across ticks (no duplicate package, no budget churn); an already-planned
  or plan-locked Brief — not a pending package — is left to start normally rather than
  stalling.
- **Unknown values fall back to `tail`** (a typo never silently turns on preemption). The
  effective trigger is surfaced on the tick record as `plan_package_trigger`
  (`tail` / `before_execute`).

**Authoring still NEVER approves on its own.** `before_execute` is the active *planner*,
not an autonomous *approver*: the authoring layers (F-bis / F-ter) still never accept their
own `confirm`, never assign agents / pick tools / create children, and always leave the
confirm OPEN. Acceptance is a **separate, grant-gated** authority — F-quater below — never
implied by authoring. This is **not** freeform LLM control of the company — it is a bounded,
governed proposal placed one step earlier in the tick.

### F-quater. Prime Plan-Package Approval — Standing Authority v1 (opt-in, grant-gated)

**The gap.** F-bis/F-ter let Prime *author* a plan package but always left the `confirm`
OPEN for a human — so even a Prime-authored decomposition stalled until a person clicked
Approve. That was the right default while no authority existed, but it left the loop unable
to carry a package it had itself proposed all the way to materialized children, even when
the Board wanted exactly that.

**The contract.** Acceptance of a Prime-authored package is now possible, but **only**
through an explicit, Board-granted standing authority — never as a side effect of authoring.
This is the seventh standing-authority category (alongside §G's review/apply), with the same
"operate *through* the grant, not around the gate" rule:

- **`prime.plan_package.approve` — the grant.** When — and ONLY when — a live
  `standing_approvals` row for `__relix_autonomous_prime__` in the Guild covers this
  category, the autonomous/manual tick will ACCEPT/materialize an **OPEN plan-package
  confirm that autonomous Prime itself authored** (confirm/suggestion `author` =
  `__relix_autonomous_prime__`). With no grant the confirm stays OPEN exactly as in
  F-bis/F-ter (a pending `before_execute` package keeps holding the start).
- **Existing path + exactly-once ledger.** Acceptance flows through the EXISTING
  `TaskStore::respond_plan_confirm` primitive — the SAME one the human
  `brief.plan_confirm_respond` handler calls — so the linked proposal is materialized once
  through the exactly-once decomposition ledger (§1.7). No hand-rolled child creation, no
  ledger bypass. Children open **unassigned** (an autonomous package carries no assignee
  hints), so the resolved-assignee set is empty.
- **Prime-authored packages ONLY.** A `confirm` authored by a human or any other actor is
  never auto-approved, even with the grant — the gate matches on the synthetic authority id.
- **Ordering — an actionable governance gate.** The approval runs **before** opening a
  duplicate package and **before** any raw start. A pending Prime-authored package is the
  next relevant gate the moment the grant exists; without the grant the prior hold/report
  behaviour is unchanged.
- **Bounded + tenant-scoped + idempotent.** Single-Brief candidate Mandates only; a grant
  in Guild A never approves Guild B's package. One bounded grant call is consumed **only**
  on a real materialization (`max_calls` / `max_cost_micros`; an unlimited grant is not
  decremented). Once accepted the confirm is `resolved`, so a re-tick neither duplicates
  children nor consumes a second grant. A stale/refused accept is recorded honestly with no
  grant consumed. The tick record carries the action `plan_package_approve` (outcome
  `advanced` on success) with the `suggestion_id` / `confirm_id` / `child_count`.

**Still NOT blanket self-approval.** This is explicit standing authority for **Prime-authored
packages only**, through the existing confirm/decomposition-ledger path only — not a code
path that lets Prime approve arbitrary work, another actor's package, or anything beyond the
bound the Board set. With no `prime.plan_package.approve` grant every plan-package gate stays
human, exactly as before.

### G. Prime Shift Disposition v1 — autonomous review-accept + apply (opt-in, grant-gated)

**The gap.** The autonomous Prime loop could plan, staff, orchestrate, prioritize,
and **start** ready work, but a *completed* Shift (a `done` run sitting in
`pending_review`) stopped dead waiting on a human to accept its review and apply
it — the review→done tail (§12.6, the `run.apply` review-to-done above) was
always a human's. So "Prime is autonomous" still had a visible seam at the end of
every Shift.

**The contract.** Under **two SEPARATE, default-OFF standing authorities** the
loop may now close that tail on the Board's behalf — but only through the EXISTING
review/apply code paths and all their safety, never a hand-rolled file copy, and
never combined into one broad power:

- **Two separable grants.** `prime.run.review_accept` lets the loop **accept** a
  completed Shift's review; `prime.run.apply` lets it **apply** an already-accepted
  run. They are distinct `standing_approvals` categories on the synthetic
  `__relix_autonomous_prime__` authority, granted/revoked through the SAME
  `agent.standing_approval.*` routes + Settings card as the other categories. Both
  default OFF. **Review and apply are separate ticks:** a single tick accepts XOR
  applies one run (the first tick accepts; the next applies).
- **Only attributable runs.** A candidate run must belong to the candidate
  Mandate/proposal's **own Brief set** (tenant-scoped by construction + an explicit
  `run_belongs_to_tenant` guard), so a cross-tenant run is **invisible** and an
  arbitrary Action Center run is never selected. Selection is deterministic
  (oldest-first by `(started_at, run_id)`; apply takes precedence over a fresh
  accept so half-integrated work closes first).
- **Eligibility is computed, never modelled.** `review_accept` requires the Brief's
  latest run to be exactly `done` + `pending_review` (so a
  failed/refused/interrupted/running/cancelled/continued or
  rejected/discarded/accepted/applied run is never accepted). `apply_run` requires
  `done` + `accepted` + apply status not already `applied`/`discarded`/`conflicted`/
  `failed` + the existing `run_apply_eligibility` to pass.
- **Execution reuses the exact manual paths + safety.** Accept goes through the
  existing review path (`TaskStore::set_run_review` with `accepted`); apply goes
  through the EXACT manual `run.apply` body (`execute_run_apply`) — re-running
  `run_apply_eligibility`, the baseline-hash / conflict / artifact-safety plan, and
  (only on a clean `applied`) the review-to-done `complete_reviewed_brief`. A
  **conflicted or failed apply records `blocked`, never marks the Brief done,
  consumes no grant, and is not retried in the same tick.**
- **Same model seam as §C/§E.** The optional deliberation/prioritization layers may
  only confirm/hold/order these already-computed legal actions — the model never
  decides eligibility, invents a run id, bypasses a grant, or bypasses apply safety.
  A `none`/hold causes zero side effects. Each successful action consumes one bounded
  grant call and chronicles `prime.autonomous_review_accept` / `prime.autonomous_apply`.

With neither grant, the review and apply gates stay a human's exactly as before —
this only *adds* the closing power when the Board has explicitly granted it, inside
the bound it set.

---

## 12.6 First-run company bootstrap + starter crew (the empty-company on-ramp)

**The gap.** A brand-new Guild has no Operatives. The Board can already stand
up the apex **Founder** (`company.bootstrap_founder` → `POST
/v1/spine/company/init`), and the Prime loop (§12.5) plans honestly — but on a
truly empty company every planned Brief lands **unassigned** (no active
work-role Operative exists), so `prime.start` correctly skips everything and
the operator never sees a *single* Shift run. That is honest, but it is not a
satisfying first run: there is no governed path from "empty company" to "at
least one real Shift completes" without first installing and authenticating an
external coding-agent CLI.

**The contract — `company.starter_crew`.** A new, governed first-run
capability (`company.starter_crew` → `POST /v1/spine/company/starter-crew`)
that turns an empty company into a *minimal, runnable* company **for safe,
local work only**. It is the Board's sovereign on-ramp (§5.4), not autonomy:

- **Owner-gated.** Same gate as `company.bootstrap_founder`: the caller must be
  a real operator/admin **or** carry the boot-seeded operator-console
  (`allow-all`) identity — i.e. it IS the trusted dashboard owner (the Board).
  A normal Operative is refused. At the bridge it additionally requires a
  logged-in dashboard session (defence in depth, exactly like
  `/v1/spine/company/init`).
- **Direct active creation is acceptable HERE.** Unlike a CEO/Operative spawn
  (which must mint a `pending`-inert hire behind a Clearance, §4.4/§5.5), the
  Board provisioning its own starter crew is a sovereign first-run action — the
  same trust basis under which `ensure_founder` creates an **active** Founder
  and `agent.create` is operator-only. So `company.starter_crew` may create
  **active** starter Operatives directly. This is the *only* place direct
  active creation of a work-role Operative is allowed, and only for the owner.
- **Safe + local + clearly labelled.** Starter Operatives are bound to the
  built-in **`echo`** Rig (a local, no-external-call reference adapter) and are
  named/titled unmistakably as local/safe/demo crew (e.g. *"Starter Engineer
  (local · echo)"*). They are **never** presented as Claude/Codex or any real
  provider. Running them costs nothing and reaches no network.
- **Idempotent.** Re-running ensures (never duplicates) the Founder and one
  starter Operative per requested role: a starter for a role that already
  exists is returned, not re-created. Safe to call repeatedly.
- **Tenant-scoped.** Everything is created in the caller's Guild; a second
  Guild gets its own independent starter crew, with no cross-tenant leak.
- **No gate is bypassed.** Key/Allowance/tenant enforcement is unchanged: the
  starter Operatives are ordinary active Operatives (a worker has no
  spawn/assign Keys), the single-owner **Claim** still prevents double-work,
  and the per-Operative Allowance hard-stop still applies on the autonomous
  heartbeat path. `company.starter_crew` hires no one behind the Board's back,
  runs no adapter, and changes no budget — it only *provisions* the crew.
- **Default roster.** By default it ensures an **engineer** and a **designer**
  (the two tracks the flagship "build" plan uses), so a "build …" proposal's
  tracks + the integrate Brief all become assignable and runnable. The role
  set is overridable by the owner; roles are canonicalised and de-duplicated
  and the count is capped.

**The positive local loop (no external auth required):**
*Initialize / starter crew* (`company.starter_crew` — Founder + safe local
echo Operatives) → describe in Chat → `prime.propose` → **Approve & create**
(`prime.approve` assigns the tracks to the active starter Operatives) →
**Start the work** (`prime.start` runs the ready Briefs through the echo Rig) →
each Shift reaches `done` and opens review on the board / Action Center. Every
step is still a governed gate; the only thing `company.starter_crew` adds is a
governed way to *populate* the crew with safe local workers so the loop can
actually close on a fresh company.

**Runnable on approval (the missing-role hire path).** A freshly-filed Prime
hire (the `pending` role-track Operative `prime.approve` files) carries **no
Rig**, so activating it alone would leave it active-but-un-runnable until a Rig
is configured. `agent.approve_hire` (`POST /v1/agents/:id/approve-hire`)
therefore accepts an **optional `rig`** and binds it **atomically at approval**:
one governed call activates *and* rigs the Operative so `prime.start` can run
its track immediately — no separate "switch the Rig" step. The Rig is validated
against the known-Rig allowlist (so a typo can't activate onto a Rig the
dispatcher would silently fall back from); `echo` (the safe-local built-in) is
always accepted. This is **not** a silent assignment of a paid/interactive CLI:
the operator passes the Rig explicitly (the Action Center hire card *suggests*
the safe-local `echo` and carries the machine-actionable approval target), a
duplicate/conflicting approval never clobbers an already-bound Rig, and omitting
`rig` preserves the prior behaviour with the response flagging `needs_rig` so
the operator knows a Rig is still required.

**Remaining gap (honest).** This closes the loop for **safe local** work only.
Real Claude/Codex-authenticated execution still requires the operator to
install + log in to a coding-agent CLI (Settings) and choose that Rig for the
Operative (at approval, or later via `agent.update {rig}`) — neither
`company.starter_crew` nor `agent.approve_hire` provisions or authenticates any
external adapter; they only ever bind the safe-local `echo` unless the operator
explicitly names another installed Rig.

---

## 13. Glossary

- **Company** — your organization; the top-level container (a tenant, product-faced).
- **Board** — you, the human owner; sovereign governance authority.
- **Goal / Initiative** — a durable high-level outcome; the "why."
- **Project** — a workstream grouping issues under a goal.
- **Issue** — the atom of work *and* its conversation; one assignee, a status, a thread, sub-issues, documents, blockers, runs.
- **Sub-issue** — a child issue created to break work down and delegate it.
- **Run** — one working episode of an agent on an issue, with a transcript and cost.
- **Agent / employee** — an AI worker with identity, a job, a boss, permissions, a budget, and a lifecycle.
- **CEO** — the apex agent; takes a goal and assembles/runs a team within granted powers.
- **`reports_to`** — the single link from an agent to its boss; builds the org tree.
- **Org chart** — the visual tree of the company; also a governance surface.
- **Chain of command** — the path *up* the org tree (escalation).
- **Manager subtree** — everyone *below* a manager; the scope unit for delegated authority.
- **Permission / power** — a granted ability (spawn agents, assign work, use a tool, access a secret, act at a risk level). Default-deny.
- **Scope** — a bound on a power (a project, a list of agents, or a subtree).
- **Approval gate** — a point where the Board (or a delegated approver) must say yes: hire, strategy, budget override, high-risk action.
- **Standing approval** — a pre-granted, time/count/spend-bounded yes.
- **Heartbeat / wake** — the event that starts an agent working (assignment, mention, timer, a cleared dependency).
- **Checkout** — taking exclusive ownership of an issue's execution; prevents double-work.
- **Blocker** — a first-class dependency: this issue can't start until that one is done.
- **Billing code / request depth** — cross-team cost attribution and delegation-hop tracking.
- **Chat companion** — the context-aware reasoning surface that turns conversation into work objects.
- **Inbox** — the Board's action center: what needs you, right now.

---

*End of design. The next step is execution against the roadmap in Section 11; the idea layer above is the contract every phase checks itself against.*
