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

The throughline: **approvals are uncircumventable** — there is no code path that lets a "requires approval" action proceed without a valid approval. (This is already true in Relix's gate; we keep it.)

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
  start; it does not propose, approve, staff, or loop on its own. It is the
  governed trigger that lets the heartbeat/assignment loop (§6.1) begin for a
  planned Mandate in one step instead of Brief-by-Brief.

**The closed loop:** describe in Chat → `prime.propose` (a request-aware
plan, nothing created) → **Approve & create** (`prime.approve` — Mandate +
Briefs + assignments + pending hires) → greenlight any Clearances → **Start
the work** (`prime.start` — the ready Briefs become real Shifts) → watch the
runs finish on the board. Every step is a governed gate; nothing runs itself.

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
