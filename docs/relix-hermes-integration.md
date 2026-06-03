# Relix × Hermes — Integration Design (embed the worker, plug in the bridge, fold in everything)

> **Status:** Design / idea layer. *What we're building and why*, not *how the code is written* — names/schemas/routes will change; the **ideas** here are the contract. No code in this document.
>
> **Grounding:** based on a complete, file-by-file read of the entire Hermes Agent codebase (~1.5M+ LOC — agent loop, tools, gateway + ~18 platform adapters, the plugin ecosystem, the Electron/Tauri/web front-ends, cron, ACP, the provider-profile system, and the ~504K-line test suite that documents Hermes's hard-won behavioral invariants). Where this doc says "Hermes does X," it was verified at the source level.
>
> **Note on scope (read this first):** this document designs the **Hermes adapter** — the deepest, richest agent backend in Relix. It lives *inside* a more general system: [`relix-agent-adapters.md`](relix-agent-adapters.md), the universal "plug in any agent" layer where Hermes is one adapter among many (Claude Code CLI on your Max subscription, Codex CLI on your ChatGPT subscription, ACP agents, remote APIs, etc.). Read the adapter doc for the frame; read this for Hermes in full. Everything here still holds — it's "the flagship adapter, in detail."
>
> **Companion docs (read together):**
> - [`relix-agent-adapters.md`](relix-agent-adapters.md) — **the universal agent-adapter system this doc sits inside.** Plug in any agent; use installed Claude/Codex subscriptions.
> - [`relix-company-model.md`](relix-company-model.md) — the company/work-object spine (Company→Goal→Project→Issue→Run), org, permissions, governance.
> - [`relix-execution-and-issue-design.md`](relix-execution-and-issue-design.md) — the Issue object + heartbeat/assignment + supervisory loop.
> - [`relix-dashboard-design.md`](relix-dashboard-design.md) — the operator console + chat companion.
> - [`hermes-vs-paperclip-vs-relix.md`](hermes-vs-paperclip-vs-relix.md) — the three-way comparison this document acts on.
>
> **This document is the answer to two questions:**
> 1. *"Take everything from Hermes and put it into our plan — every single thing."*
> 2. *"What if we have Hermes already installed, and Relix just plugs into it?"*
>
> The short version: **we do exactly that.** Relix becomes the secure building and the company; an installed Hermes becomes the brain of each worker; a thin **Relix bridge plugin** living inside Hermes is the wire between them. We don't fork Hermes and we don't rebuild it — we *govern* it.

---

## 0. The one-paragraph thesis

Hermes is the best self-improving single agent in existence and ~1.5M lines of battle-tested worker machinery. Rebuilding it in Rust would take years and we'd still be behind. So we **don't**. Instead: **Relix builds the spine, the governance, and the security natively (that's our identity), embeds an installed Hermes as the execution brain of each agent, transplants Hermes's hard-won *patterns* into our native spine, and connects the two with a plugin.** The result is the thing none of the three reference systems is alone — a **secure mesh (Relix) running a governed company (Paperclip-model) staffed by self-improving employees (Hermes), where the employees are literally Hermes processes Relix supervises and gates.**

---

## 1. Three ways to use Hermes — and the one we choose

There are three honest strategies. We should name all three so the choice is deliberate.

| Strategy | What it means | Verdict |
|---|---|---|
| **A. Transplant** | Reimplement Hermes's good ideas natively in Relix's Rust. | Full control, but enormous effort, and we'd re-derive years of self-teaching-loop maturity. **Right for the *spine*, wrong for the *worker brain*.** |
| **B. Embed** | Run an installed Hermes as the worker runtime; Relix orchestrates and governs it. | Fast — we get the entire worker (loop, self-teaching, execute_code, terminal backends, provider quirks) almost for free. **Right for the *worker brain*.** |
| **C. Plug-in** | A Relix plugin lives *inside* each Hermes so Hermes reports up to Relix, asks Relix for approvals, and routes sensitive tools back through Relix's gate. | This is the *glue* that makes B feel native instead of a black box. **The founder's idea — and the keystone.** |

**Decision: we do B + C, with A reserved for the spine and for transplanting patterns.** Concretely:

- **Native Relix (Strategy A territory) — the spine + governance + security.** Company/Goal/Project/Issue/Run ledger, the org tree, permissions, approvals, budgets, the heartbeat/assignment loop, the dashboard, the chat companion, the signed mesh, the admission pipeline, audit, tenant isolation. **This is Relix's identity and differentiator. It must be native and must never be Hermes.**
- **Embedded Hermes (Strategy B) — the worker brain.** The agent loop, the self-teaching/curator loop, `execute_code`, the six terminal backends + serverless hibernate/wake, provider-quirk handling, memory operational rigor, prompt-caching discipline, the trajectory flywheel. **Hermes already has these, tested in production. We use them as-is.**
- **The Relix bridge plugin (Strategy C) — the wire.** A plugin we ship *into* Hermes that makes an installed Hermes a first-class Relix worker node: it pulls assignments from the Issue ledger, streams progress back as Issue comments, routes Hermes's tool calls and approvals through Relix's gate, and feeds Hermes's learning loop with tenant-scoped memory.
- **Transplanted patterns (Strategy A territory, but learned from Hermes) — the invariants that shape our native spine.** The work-kernel CAS rules, the session-concurrency fix, credential hygiene, compression locks, memory lifecycle, kanban lease/heartbeat/circuit-breaker — Relix-native code, but the *design* comes straight from Hermes's tests.

> **The mental model in one line:** *embed the runtime, plug in the bridge, transplant the patterns, build the spine native.*

> **Founder's refinement (2026-06-02) — native-first for the top capabilities.** The standout Hermes capabilities — the **self-improving learning loop**, **`execute_code`**, the **sleep-and-wake workspaces**, the **rich plugin system**, the **memory discipline** — are to be **built natively into Relix (Strategy A / Transplant), as Relix's own features**, not merely borrowed from an embedded Hermes. We have the complete blueprint from the every-line read, so we know exactly how to build them. Embedding Hermes is then **two optional things, not the source of Relix's powers**: (1) a *fast bootstrap* — run Hermes to get a capability working immediately while we build the native version, and (2) *the deepest adapter* in the universal plug-in layer ([`relix-agent-adapters.md`](relix-agent-adapters.md)). So in the §4 disposition table below, the **top capabilities move toward "Transplant (native)" as the primary plan**, with "Embed" demoted to bootstrap/adapter. **Relix is excellent on its own; plugging in Hermes (or any agent) is upside, not a dependency.**

---

## 2. The architecture — Hermes as a Relix worker node (one adapter among many)

### 2.1 The new node type: the Hermes worker node

Relix already has node types (memory, ai, tool, coordinator, web-bridge, channels, plugins). We add one: a **Hermes worker node** — a Relix-supervised process that *is* an installed Hermes, wrapped in a Relix node identity.

- It has a **signed mesh identity** like any Relix node, so everything it does on the mesh is identity-checked and audited.
- It runs **inside Relix's isolation** (the same jail/sandbox posture the tool node already uses): its own process, its own filesystem box, its own scoped environment.
- It is **one Hermes per agent (or per tenant)** — see §3.3. Each gets its own `HERMES_HOME` (Hermes's profile system already supports per-`HERMES_HOME` isolation natively), its own skills/memory/credentials, its own identity.
- Relix **supervises its lifecycle**: spawn on demand, health-check, restart on crash (Hermes has strong crash-recovery of its own), hibernate when idle, terminate on agent termination.

The coordinator treats this node like any other executor: when an Issue's Run needs to happen, the coordinator routes it to the agent's Hermes worker node. **Hermes does the *doing*; Relix does the *deciding, governing, and recording*.**

### 2.2 The seam — how Relix talks to an installed Hermes

We do **not** fork Hermes. Hermes already exposes several programmatic front doors; we ride the cleanest ones. Three channels, each with a job:

**(a) Run dispatch & events — Hermes's structured-runs API (`/v1/runs`).**
Hermes's built-in API server already speaks exactly the contract we need for "assign a unit of work, watch it, broker approvals, get the result":
- POST a run → get a `run_id` back immediately (non-blocking).
- Stream events: `tool.started` / `tool.completed` / `reasoning.available` / `message.delta`.
- An **approval endpoint** (`once` / `session` / `always` / `deny`) — a pending dangerous action blocks until answered.
- A **stop endpoint** — interrupt a running agent.
- **Idempotency keys** — so a retried dispatch never double-runs.

This *is* the Run abstraction from the company model, already implemented. Relix's coordinator POSTs a Run to the agent's Hermes node, subscribes to its events (which become the Issue's live transcript), brokers approvals through Relix's Inbox, and records the outcome + cost in the Issue ledger.

**(b) Gated capabilities — Relix exposes its mesh tools to Hermes over MCP.**
Hermes is a first-class **MCP client**. So Relix stands up an MCP surface that exposes its *gated mesh tools* (anything that touches the mesh, other nodes, secrets, real money, production) to the embedded Hermes. When Hermes calls one of those tools, the call **routes back through Relix's admission pipeline** (identity → policy → handler → hash-chained audit). This is the mechanism that keeps the security boundary even though Hermes is the brain: **the powerful tools aren't Hermes's, they're Relix's, lent to Hermes under the gate.** (Hermes keeps its *local, in-box* tools — edit files in its workspace, run code in its sandbox — for itself; only boundary-crossing capabilities route back.)

**(c) Control & identity — the Relix bridge plugin (the founder's plug-in).**
Channels (a) and (b) make Hermes *drivable*. The bridge plugin (§2.3) makes Hermes *Relix-aware* — it reports up, asks for permission, and writes its memory/learning back into Relix's stores. This is what turns a black-box Hermes into a native employee.

> **Why a seam at all (the insurance):** by making `/v1/runs` + MCP the integration contract, the embedded Hermes is *swappable*. If Relix ever wants a native Rust worker for some agents, it implements the same Run/event/approval contract and slots in behind the same seam — no change to the spine. We get Hermes's power now without marrying it forever.

### 2.3 The Relix bridge plugin — "plug in using a plugin"

This is the founder's idea, designed. Hermes's plugin system is exactly the right shape for it: a plugin is a directory with a manifest plus a single `register(ctx)` entry point, and it can register **lifecycle hooks** that intercept the agent loop. We ship one plugin — **`relix-bridge`** — into every Hermes worker. It registers:

- **A "Relix" platform adapter** — so Hermes treats the agent's **Issue thread as a conversation channel**. An assignment arrives as an inbound message ("here is Issue #482, its description, its goal, its prior run context"); the agent's progress comments, questions, and results go *out* as messages that Relix lands in the Issue thread. The agent literally "talks to its issue," natively, because to Hermes the Issue is just another chat channel.
- **A `pre_tool_call` hook → Relix's gate.** Before Hermes runs *any* tool, the hook asks Relix's policy gate "is this agent allowed to do this, at this risk level, with these scopes?" A denial blocks the tool with a readable reason. This extends Relix's responder-enforced policy *inside* Hermes, covering even Hermes-internal tool calls — defense in depth on top of the MCP-routing in §2.2(b).
- **A `pre_approval_request` hook → Relix's Inbox.** When Hermes hits a dangerous action, instead of prompting a local terminal it raises a Relix approval — which surfaces in the Board's Inbox as an answerable card, gets a signed one-shot approval token, and unblocks Hermes. (Relix already mints signed, one-shot, standing approvals; the hook just points Hermes at them.)
- **A memory-provider plugin → Relix's four-layer memory.** Hermes's memory writes (and its self-taught skills) land in Relix's tenant-scoped memory store instead of a local SQLite file — so memory is governed, isolated per tenant, and visible in the dashboard. Hermes's `MemoryProvider` ABC is a clean 12-method contract built exactly for this kind of swap.
- **A `post_tool_call` / `on_session_end` hook → Relix's cost + audit + learning surfaces.** Every tool result emits a cost/audit event into Relix's ledger; the end-of-run summary writes the Run record, rolls cost up the work tree, and triggers Relix's side of the learning loop.

The beauty: **all five of these are things Hermes's plugin API already supports natively.** We're not bending Hermes — we're using its own extension doorway, exactly as it was designed, to make it a Relix citizen.

### 2.4 The full request flow (end to end)

How a single piece of work flows through the integrated system:

1. **You (or the chat companion, or the CEO) create an Issue** and assign it to an agent. (Native Relix — the spine.)
2. **The heartbeat loop wakes the agent** and **atomically checks out** the Issue (single-owner; no double-work). (Native Relix — transplanted from Hermes's kanban claim semantics.)
3. **The coordinator dispatches a Run** to the agent's **Hermes worker node** via `/v1/runs`, handing it the Issue (as a channel message through the bridge plugin) with its description, goal ancestry, and prior-run context.
4. **Hermes does the work** — its loop, its self-taught skills, `execute_code` for mechanical glue, its terminal backend for the sandboxed doing. Local tools it runs itself, in its box.
5. **Boundary-crossing tool calls route back through Relix** over MCP and the `pre_tool_call` hook → identity → policy → audit. Anything dangerous **raises a Relix approval** that surfaces in your Inbox.
6. **Progress streams back as Issue comments + a live transcript** (Hermes's `/v1/runs` events → the bridge → the Issue thread). You watch it work, on the issue.
7. **The Run ends.** The bridge writes the **Run record, cost (rolled up the work tree), and audit**; if the agent created sub-issues, the **exactly-once decomposition** lands them as child Issues. (Native Relix spine.)
8. **The supervisory wakes fire** — children-completed / blockers-resolved wake the parent (the planner) to review and assign the next slice. (Native Relix — transplanted from Hermes's event-driven kanban + Paperclip's pattern.)
9. **Off the critical path, Hermes's learning loop runs** — its background-review fork decides whether to save a new skill; the curator ages/curates the skill library; the memory nudge updates the user/company model. All of it writing into Relix's tenant-scoped, governed memory via the bridge.

**Relix owns steps 1–3, 5 (the gate), 7–8 (the spine + governance). Hermes owns steps 4, 6, 9 (the doing + the learning).** Clean division.

---

## 3. Security reconciliation — the box *is* the boundary

This is the most important section, because Hermes's security philosophy is the *inverse* of Relix's, and naïvely embedding it would throw away Relix's whole value.

### 3.1 The philosophical clash, resolved

Hermes's stated security model (from its `SECURITY.md`): **single-tenant, and "the only security boundary against an adversarial LLM is the operating system."** Everything in-process — its approval gate, output redaction, pattern scanners, tool allowlists — is explicitly a *heuristic, not containment*. Its plugin/skill trust model is "operator reviews before install."

Relix's model is the opposite: **cryptographic identity + responder-enforced policy + hash-chained audit + tenant isolation** — real boundaries, multi-tenant.

These don't conflict — **they compose perfectly, because Hermes *assumes* an OS boundary it doesn't provide, and providing that boundary is exactly what Relix does.** So:

> **Relix puts each Hermes in a box, and that box is the boundary Hermes was designed to live inside.** Hermes gets to be its fully-capable, "no in-process containment needed" self — *because* it's already sealed inside a Relix-governed sandbox with a signed identity, scoped credentials, and a policy gate on everything that crosses the box wall.

### 3.2 What Hermes does itself vs. what routes back through the gate

The dividing line is **the box wall**:

- **Inside the box, Hermes acts freely:** edit files in its own workspace, run code in its own sandbox/terminal backend, use its local reasoning tools, manage its own skills/memory. None of this can hurt anyone else because the box contains it. This is where Hermes's speed and capability live — don't gate it.
- **Crossing the box wall always routes through Relix's gate:** touching the mesh, calling another node, reaching another tenant, using a real secret/credential, spending money, deploying to production, sending external messages. These are exposed to Hermes **as Relix MCP tools** (§2.2b) and double-checked by the **`pre_tool_call` hook** (§2.3) — so every boundary-crossing action is identity-signed, policy-gated on the responder, and audited. Hermes can't escalate past the box because the powerful tools aren't its own.

This is the same posture Hermes itself documents as the correct one ("whole-process wrapping when ingesting untrusted surfaces") — Relix just makes it the *default, enforced* posture instead of an operator's optional choice.

### 3.3 Tenant isolation — one Hermes per tenant/agent

Hermes is single-user by design. We turn that *constraint* into a *clean isolation primitive*:

- **One Hermes worker instance per agent (or per tenant), each a single-tenant unit.** Each has its own `HERMES_HOME`/profile (Hermes's profile system already gives per-`HERMES_HOME` isolation of skills, memory, credentials, cron, sessions — natively, with a cross-profile write guard). Each has its own Relix signed identity and scoped credentials.
- **Relix's mesh keeps them from seeing each other** — they communicate only through the gated mesh, never directly.
- **This sidesteps the pending tenant-isolation gap.** Our memory note ([`project-tenant-isolation-part3-pending`]) flags that Relix's production skill/session/credential stores still lack tenant filtering. For Hermes-backed agents, that gap is *structurally* closed: each Hermes is already a single-tenant box with its own stores. The native Relix stores still need the tenant-aware migration for the non-Hermes paths, but embedding Hermes per-tenant means we don't depend on it for the worker layer.

### 3.4 Credential hygiene (transplant Hermes's own discipline)

Hermes's tests pin exactly the credential invariants Relix's tenant work needs — and we apply them at the box wall: secrets are usable in-memory but **stripped at every disk/transport boundary** (only `sha256:` fingerprints persist); provider keys are scrubbed from the child process env by default; credential writes are atomic `O_EXCL` mode-0600; "I removed this key" is durable across reloads (suppression markers); token endpoints are host-allowlisted so a poisoned config can't exfil. Relix supplies each Hermes box its scoped credentials through the vault, never the raw company keyring.

---

## 4. The three-way split — every Hermes capability, placed

The master table. Every significant Hermes capability, and where it lands: **Native** (Relix builds it), **Embedded** (we get it from the Hermes runtime), or **Pattern** (Relix-native, designed from Hermes's tests).

| Hermes capability | Disposition | Where it lives in Relix |
|---|---|---|
| Agent loop (reason→tool→observe) | **Embedded** | The Hermes worker node's own loop |
| Self-teaching skill loop (background-review fork) | **Embedded** | Hermes runs it; writes skills into Relix memory via the bridge |
| The curator (age/consolidate/never-delete, provenance gate) | **Embedded** | Hermes's curator, over Relix-stored skills |
| The memory nudge (post-response review) | **Embedded** | Hermes |
| `execute_code` (RPC-from-script, budget-refunded) | **Embedded** | Hermes's tool; sandboxed in the box; gated for boundary-crossing RPC |
| 6 terminal backends + serverless hibernate/wake | **Embedded** | Hermes's backends *become* Relix's execution-workspace layer |
| Provider-quirk handling / ProviderProfile / api_mode transports | **Embedded** | Hermes handles model wire-protocol; Relix's tier router still routes *which* model |
| Memory-context fencing (untrusted-recall security) | **Embedded** + **Pattern** | Hermes does it locally; Relix applies the same fence at mesh memory boundaries |
| Auxiliary-client cost/health router | **Embedded** | Hermes's side-LLM router for compression/vision/titles |
| LLM-free FTS5 session search + bookends | **Embedded** | Hermes |
| Prompt-caching discipline | **Embedded** | Hermes builds prompts; Relix's heartbeat respects byte-stability |
| Cron cheap-precheck → conditional-wake (`no_agent` + wakeAgent gate + `context_from`) | **Pattern** | Relix's routines adopt the two-tier wake; can also delegate to Hermes cron |
| Trajectory training flywheel | **Embedded** | Hermes captures runs; Relix's training pipeline (Hermes lineage) consumes |
| Plugin extension model (`register(ctx)` + 21 hooks + kind-routing) | **Pattern** | Relix's own plugin/extension surface learns this shape; *and* we ship the bridge plugin into Hermes |
| Kanban resilience (lease+heartbeat, circuit-breaker, ownership, activity→heartbeat) | **Pattern** | The native heartbeat/assignment loop |
| Work-kernel CAS invariants (claim-once, fan-in gate, sticky-block, hallucinated-card guard) | **Pattern** | The native Issue/Run ledger |
| Session-concurrency fix (sentinel-before-await + generation counter) | **Pattern** | The native heartbeat loop + chat companion (follow-ups racing a run) |
| Per-session compression lock (no transcript fork) | **Pattern** | Native session handling |
| Memory provider lifecycle (real transcript on end, drain on shutdown) | **Pattern** | Native memory node contracts |
| Diagnosis-driven recovery (retry-once→escalate, never-auto-reassign, Inbox decision card) | **Pattern** | Already in the execution design; reinforced |
| Multi-surface thin client (REST + JSON-RPC-WS + raw-PTY) over one core | **Pattern** | Relix dashboard (retire vanilla `dashboard.html` for the React SPA) |
| Supply-chain hardening (exact-pin, `[all]`-excludes-lazy, signed artifacts, OSV-before-launch) | **Pattern** | Relix's dependency/CI policy |
| Product shipping (thin signed installer that clones+builds a pinned agent) | **Pattern** | Relix's packaging/distribution |
| The work-object spine, org tree, permissions, approvals, budgets, dashboard | **Native (never Hermes)** | The company model — Relix's identity |
| The signed mesh, admission pipeline, tenant isolation, audit | **Native (never Hermes)** | The substrate Hermes runs *inside* |

---

## 5. Everything from Hermes, folded in — the complete catalog

Per the instruction "include every single thing," here is the exhaustive list of what we take, each with *what it is*, *how it lands*, and *which phase*. (Phases reference the company-model §11 roadmap, extended in §7 below.)

### 5.1 The crown jewel — the closed self-improving learning loop *(Embedded; Phase H3 / company Phase 7+)*
- **What:** after a tool-intensity-triggered moment, a background fork of the agent (inheriting the parent's cached prompt, ~26% cheaper) decides whether to write a new skill; the curator ages skills on a usage clock (active→stale→archived at 30/90d), consolidates siblings into umbrellas, **never deletes**, and is provenance-gated (`created_by:agent` — can't touch user-authored skills); a counter-based nudge runs the review *after* the response so it never competes with the task.
- **How it lands:** **we get the whole loop for free from embedded Hermes.** What we add: the bridge points its skill/memory writes at Relix's **tenant-scoped, governed** memory store, and the dashboard surfaces "skills this agent taught itself" on the agent page. This is Relix's long-wanted "organizational learning," delivered by reuse instead of a multi-year build.

### 5.2 `execute_code` — RPC-from-script *(Embedded; Phase H2)*
- **What:** the model writes one script that calls tools over RPC; only stdout returns; the turn is budget-refunded — an N-step deterministic pipeline collapses to ~one near-free turn, and the RPC re-enters the same tool dispatch so security still fires.
- **How it lands:** Hermes's tool, used as-is inside the box. Its boundary-crossing RPC calls route back through Relix's gate (same as any tool). The cheapest "deterministic glue" primitive on the platform, for free.

### 5.3 Serverless execution backends *(Embedded; company workspace phase)*
- **What:** six backends behind one `execute()` interface; Modal snapshots the filesystem and recreates, Daytona stops/resumes — keyed by task id, `sync_back` on teardown → hibernate to ~$0, wake with exact state.
- **How it lands:** **Hermes's backends *become* Relix's "execution workspaces" layer.** Relix's "Cloud/Sandbox agents" roadmap item is answered by configuring the embedded Hermes's backend per project/agent: shared cwd, isolated worktree, or a hibernating cloud sandbox — Relix picks the policy, Hermes runs it.

### 5.4 Memory operational rigor *(Embedded + Pattern; ongoing)*
- **Memory-context fencing** (treat recalled memory as untrusted; strip forged fences; scrub across stream chunks) — Hermes does it locally; Relix applies the *same* fence when memory crosses tenant/agent boundaries on the mesh.
- **Auxiliary-client router** (one cost/health-aware fallback chain for all background LLM work, 402-failover) — embedded.
- **LLM-free FTS5 session search with bookends** (cheap cross-session recall) — embedded.
- **Memory provider lifecycle invariants** (on_session_end gets the *real* transcript; drain pending writes on shutdown; flush to the *old* session on switch) — **Pattern**, applied to Relix's native memory node contracts.

### 5.5 Prompt-caching discipline *(Embedded + Pattern; Phase H1)*
- **What:** system prompt built once, byte-stable, never mutated mid-conversation except on compression; all ephemeral context goes in user/tool messages.
- **How it lands:** embedded Hermes already enforces it; Relix's heartbeat loop must hand work to Hermes in a cache-stable way (don't perturb the system prompt per wake — a real cost lever).

### 5.6 The cron cheap-precheck → conditional-wake pattern *(Pattern; routines phase)*
- **What:** `no_agent` script jobs (zero-LLM bash watchdogs), a **wakeAgent gate** (the cheap script decides whether to spin the expensive agent), `context_from` chaining (job A's output feeds job B), at-most-once scheduling with period-scaled catch-up.
- **How it lands:** Relix's routines adopt the two-tier "cheap check first, only wake the model if needed" pattern. We can also *delegate* cron to the embedded Hermes (it has a mature scheduler), gated by Relix.

### 5.7 The trajectory training flywheel *(Embedded; training phase)*
- **What:** every run captured as a training sample; reasoning normalized to one `<think>` channel; canonical tool-schema preamble synthesized at save; zero-reasoning samples dropped; tool-stats padded to a stable schema; long trajectories compressed with the *target model's own tokenizer*, protecting head+tail.
- **How it lands:** embedded Hermes produces the samples; Relix's existing training pipeline (shared Hermes lineage) consumes them. The discipline to nail: normalization + schema-stability + tokenizer-aware compression.

### 5.8 ProviderProfile + api_mode transports *(Embedded; AI node)*
- **What:** all provider facts as declarative data (user-overridable via a plugin dir); `api_mode` → a transport class with 4 methods (convert messages / convert tools / build kwargs / normalize response). New model = a ~15-line profile; new protocol = one transport. ~22 providers, each ~15 lines.
- **How it lands:** embedded Hermes handles model wire-protocol quirks entirely. Relix's **own tier/complexity router still decides *which* model** an agent uses (Relix's router is real; don't downgrade to Hermes's plumbing-only routing) — it just hands the choice to Hermes to execute.

### 5.9 Kanban operational resilience *(Pattern; Phase 3 heartbeat loop)*
- **What:** lease + heartbeat (extend-if-alive / reclaim-if-wedged), unified failure circuit-breaker with fast-trip classes (clean-exit-without-completing = protocol violation → trip immediately), worker capability narrowing + ownership enforcement (a worker may only mutate its own task), activity→heartbeat bridge (liveness as a side effect of normal traffic).
- **How it lands:** **Pattern** — these shape Relix's native assignment/checkout loop. The *semantics* transfer; the single-host SQLite/PID transport does **not** (Relix needs the distributed claim store it already has).

### 5.10 The test-locked spine invariants *(Pattern; Phases 1 & 3 — the spine)*
Hermes's ~504K-line test suite is a written rulebook for "how a system like this must behave or it breaks," with production bug numbers attached. The ones that directly shape our native spine:
- **Work-kernel must be CAS-atomic with explicit per-attempt run rows:** claim-once-wins under concurrency; `recompute_ready` cascades and gates fan-in on *all* parents done; auto-block at the failure limit; **hallucinated-card guard** (an agent claiming credit for a nonexistent or foreign work item is caught and audited). This *is* our Issue→Run spine — they've already written down how not to screw it up.
- **Session concurrency = sentinel-before-await + generation counter:** drop a placeholder before any await so a second concurrent message is recognized as "already running" and queued (never duplicated); bump a generation counter on stop/new so a stale run can't clobber a newer slot. **Directly fixes the chat-companion case where a follow-up message races a running agent.**
- **Per-session compression lock:** prevents two paths compressing the same session from forking the transcript into orphan children.
- **Credential hygiene** (see §3.4).
- **Fail-closed external surfaces:** every network adapter needs a mandatory allowlist; an empty allowlist that fails *open* is an in-scope bug; HMAC validation before rate-limiting; session IDs are routing handles, **not** auth boundaries.
- **The `live_system_guard` test canary:** the harness blocks every `kill`/`systemctl`/`pkill` primitive so a test can never touch a live process. Worth borrowing wholesale for Relix's test suite.

### 5.11 The plugin extension model *(Pattern + the bridge; cross-cutting)*
- **What:** one `register(ctx)` entry + typed `ctx.register_*` per capability class (tool, platform adapter, LLM provider, memory provider, web/video/tts/browser backend, dashboard-auth, CLI/slash command, lifecycle hook); `kind:` in the manifest routes load policy (backend/platform/model-provider auto-load; standalone opt-in); bundled-vs-user override by directory; **21 lifecycle hooks** (pre/post tool-call, transform-tool-result, pre/post LLM-call, pre/post API-request, on_session_*, pre_gateway_dispatch, pre/post_approval).
- **How it lands two ways:** (1) **Pattern** — Relix's own extension surface adopts this clean "add a capability class without forking" shape (Relix already has a plugins node; this is the doorway design to copy). (2) **The bridge** — we *use* Hermes's plugin API to ship `relix-bridge` into every Hermes (§2.3). **Caveat to remember:** Hermes plugins run in-process with full trust (no sandbox); Relix's value-add is the real boundary Hermes skips — so the bridge is trusted-first-party, and untrusted extensions never get the same latitude.

### 5.12 Supply-chain hardening *(Pattern; dependency/CI policy)*
Copy wholesale: exact-pin-no-ranges dependencies; the `[all]` extra **excludes** lazy-installable extras so one poisoned release can't break fresh installs; SHA256-verified Docker/s6 artifacts; cosign on downloaded binaries; OSV malware check *before* launching any fetched executable (npx/uvx-style); an allowlist-only on-demand installer; a narrow high-signal CI scanner (flags `.pth` files, base64+exec one-liners, encoded subprocess args, root install hooks — and nothing low-signal, so reviewers never learn to ignore it). This was Hermes's response to the May-2026 supply-chain worm; Relix should adopt the posture pre-emptively.

### 5.13 The product-shipping model *(Pattern; packaging/distribution)*
- **What:** a thin (~5–10MB) signed/notarized native installer (Tauri) + an Electron desktop that **clone and build the agent pinned to the exact tested commit**, so CLI and GUI installs are interchangeable; a two-binary Windows self-update handoff around the venv file-lock; three parallel front-ends (Electron desktop, Ink TUI, web dashboard) all **thin clients over one gateway** via REST + JSON-RPC-over-WS + raw-PTY-over-WS, reusing the same slash pipeline rather than reimplementing it.
- **How it lands:** informs Relix's distribution and the dashboard-redesign decision (retire vanilla `dashboard.html` for the React SPA; one core, many thin faces). And — neatly — the "ship a thin installer that fetches + pins the heavy agent" model is *exactly* how Relix can distribute "install Hermes as the worker runtime" to operators: Relix's installer provisions the pinned Hermes alongside the mesh.

### 5.14 The multi-platform gateway & adapters *(Embedded; messaging)*
- **What:** one process speaking ~18 messaging platforms (Telegram/Discord/Slack/WhatsApp/Signal/Matrix/Feishu/…) behind a uniform adapter ABC, with fail-closed auth, idempotency dedup, secret-required guards, and observable-drop recovery.
- **How it lands:** Relix already has a channels node; for the platforms Hermes covers and Relix doesn't, the embedded Hermes's gateway can serve them under Relix governance — or we transplant the adapter ABC shape. Either way, the **fail-closed + observable-drop contracts** are the invariants to keep.

### 5.15 Diagnosis-driven recovery *(Pattern; already in execution design)*
- **What:** classify failures (transient → silent bounded auto-retry; hard blocker → a cheap status-only diagnostic pass that writes a plain-language root cause + recommendation → an Inbox decision card with Retry/Block/Reassign/Investigate/Dismiss); never auto-reassign; comments aren't completion.
- **How it lands:** already locked in the execution design (§3.3b there). Embedded Hermes's per-run diagnostics + circuit-breaker feed the classification; Relix renders the decision card.

---

## 6. What stays native and must never be Hermes

The line in the sand. These are Relix's identity; if Hermes ever crept into them, Relix would just be a Hermes skin and lose its entire reason to exist:

- **The work-object spine** — Company/Goal/Project/Issue/Run as durable, governed objects. (Hermes has no company; its kanban is a flat assignee string.)
- **The org tree, permissions, approvals, budgets** — real governance over many agents. (Hermes has per-task limits, not governance.)
- **The signed mesh + admission pipeline + tenant isolation + hash-chained audit** — the security substrate Hermes explicitly disclaims. **This is the box Hermes runs inside.**
- **The heartbeat/assignment loop + the durable, outlive-the-turn agent model.** (Hermes delegation is deliberately *non-durable*; Relix's whole point is "assign it and it works for days." Relix owns durability; Hermes is the executor of a single Run within it.)
- **The dashboard + the chat companion** — the goal-facing operator surface.
- **The tier/complexity router** — Relix's real model-routing stays; Hermes only handles wire-protocol once the model is chosen.

> **The test:** if a capability is about *organizing, governing, securing, or being legible to the human*, it's **native Relix**. If it's about *one agent doing the work well and getting better at it*, it's **embedded Hermes**.

---

## 7. Phasing — fold the integration into the roadmap

The company-model roadmap (its §11, Phases 0–6) stays the spine. We interleave the Hermes integration as a parallel track (Phases **H0–H4**), sequenced so each step is useful alone and nothing blocks on the embed.

- **Phase H0 — Spike the seam.** Stand up *one* installed Hermes as a supervised process; drive a trivial Run through `/v1/runs`; stream its events. Prove Relix can dispatch-and-watch. (Parallel to company Phase 0–1.)
- **Phase H1 — The bridge plugin v1.** Ship `relix-bridge` into Hermes: the Relix platform adapter (Issue-as-channel), `on_session_end` → Run record + cost, and prompt-cache-stable dispatch. Now an Issue can be worked by a real Hermes and the result lands in the ledger. (Parallel to company Phase 1–2.)
- **Phase H2 — Gate + approvals + execute_code.** Wire `pre_tool_call` → Relix's gate and `pre_approval_request` → the Inbox; expose Relix's mesh tools to Hermes over MCP so boundary-crossing actions route through admission; turn on `execute_code` inside the box. Now Hermes is *governed*, not just driven. (Parallel to company Phase 3–4.)
- **Phase H3 — Memory + the learning loop.** Point Hermes's memory provider at Relix's tenant-scoped store; let the self-teaching loop + curator + nudge run, writing governed/isolated memory; surface self-taught skills in the dashboard. This is the org-learning payoff. (Parallel to company Phase 5–6.)
- **Phase H4 — Serverless workspaces + scale.** Configure Hermes's terminal backends as Relix's execution-workspace policy (shared / isolated / hibernating cloud sandbox per project); harden one-Hermes-per-tenant supervision at scale. (The deferred workspace phase.)

Each Hermes phase rides *underneath* the corresponding company-model phase: the company model builds the spine the human sees; the Hermes track makes the worker behind each Issue real, capable, and self-improving.

---

## 8. Open decisions (resolve before/at each phase)

Honest unknowns, each genuinely the founder's call:

1. **Licensing & distribution — the gating question.** Can we embed/ship Hermes (Nous Research) under its license, for our distribution and (eventually) commercial use? **This must be answered before Phase H1.** If the license forbids redistribution, the fallback is "operator installs Hermes themselves; Relix detects + plugs into it" (still the founder's idea, just BYO-Hermes) — which actually changes very little architecturally, because the seam is the same.
2. **Embed-permanent vs. bootstrap-then-native.** Is embedded Hermes the forever worker, or a fast bootstrap we partly replace with native Rust later? **Recommendation: embed now, keep the `/v1/runs`+MCP seam clean so a native worker can slot in behind it per-agent if we ever want it.** Decide nothing permanent now; the seam is the insurance.
3. **One Hermes per agent vs. per tenant.** Per-agent = cleanest isolation + simplest mental model, but more processes; per-tenant = fewer processes, but agents share a box. **Leaning per-agent for isolation, with pooling/hibernation to control process count** (Hermes's serverless backends + idle-eviction make this cheap).
4. **Which tools route back through the gate vs. run in-box.** The default: *boundary-crossing → gated; local/sandboxed → in-box*. The exact allowlist (is "write file in workspace" ever gated? is "git push" always gated?) is a per-capability policy to settle in Phase H2.
5. **How much of Hermes's surface we expose.** Hermes has cron, kanban, ACP, a desktop app, 18 messaging platforms. Do we use Hermes's cron/messaging under Relix, or only its agent core + tools? **Leaning: agent core + tools + memory + execute_code + backends first; treat cron/messaging/kanban as Relix-native (Relix already has these) and let Hermes's versions stay dormant** to avoid two sources of truth.
6. **Version pinning & contract drift.** We pin a Hermes version and treat its `/v1/runs` + MCP + plugin-API (manifest v1) as the integration contract. How aggressively do we track upstream? (Leaning: pin, vendor the bridge plugin in our repo, upgrade deliberately.)
7. **Process supervision ownership.** Relix (Rust) supervising Hermes (Python) processes — does the coordinator own this, or a dedicated "worker-host" node? (Leaning: a dedicated worker-host responsibility, mirroring the tool node's jail.)

---

## 9. Risks & honest tradeoffs

- **A Python process boundary inside a Rust system.** Relix supervises out-of-process Hermes workers. This is *also* a feature (process isolation = a real security boundary, which is what Relix wants), but it's operational surface: lifecycle, health, restart, resource limits. Mitigated by Hermes's own strong crash-recovery and by treating the worker-host like the existing tool-node jail.
- **Dependency on an external project.** We're coupled to Hermes's wire contract. Mitigated by pinning a version, vendoring the bridge plugin, and keeping the `/v1/runs`+MCP seam swappable (a native worker can replace Hermes behind it).
- **Two memory/skill stores if we're sloppy.** If Hermes writes to its local SQLite *and* Relix has its own memory, we get drift. Mitigated by the memory-provider bridge pointing Hermes at Relix's store (one source of truth) from Phase H3.
- **Security inversion if the box leaks.** Hermes assumes the OS is the boundary; if a Hermes box isn't actually sealed (misconfigured sandbox, an in-box tool that reaches the network un-gated), Hermes's "no in-process containment" becomes a liability. Mitigated by making the box a hard Relix-governed sandbox and routing *all* boundary-crossing tools through the gate — the §3 contract is load-bearing, not optional.
- **Licensing (see §8.1).** The one that could change the plan from "embed/ship" to "BYO-install + plug in." Architecturally minor, commercially major. Resolve early.

---

## 10. Glossary additions

- **Hermes worker node** — a Relix-supervised, signed-identity, sandboxed process that *is* an installed Hermes, serving as the execution brain of one agent (or tenant).
- **The seam** — the integration contract between Relix and an embedded Hermes: structured runs/events/approvals (`/v1/runs`) + gated tools (MCP) + the bridge plugin. Deliberately swappable.
- **`relix-bridge`** — the first-party Relix plugin shipped *inside* every Hermes: registers the Issue-as-channel adapter, the `pre_tool_call`/approval/cost/memory hooks. The founder's "plug in using a plugin."
- **The box wall** — the boundary of a Hermes worker's sandbox. *Inside:* Hermes acts freely. *Crossing:* everything routes through Relix's gate.
- **Embed / Transplant / Pattern** — the three dispositions for a Hermes capability: run the actual Hermes runtime / reimplement natively / build native but designed from Hermes's tests.
- **Worker-host** — the Relix responsibility that supervises Hermes worker processes (lifecycle, health, isolation), mirroring the tool node's jail.

---

*Net: Relix stays the secure mesh and becomes the governed company — natively, that's our identity. Each employee's brain is an installed, self-improving Hermes, sandboxed inside a Relix box, driven through a clean swappable seam, made native by a bridge plugin, and governed on everything that crosses the box wall. We get Hermes's ~1.5M lines of capability without forking or rebuilding it, we transplant its hard-won invariants into our own spine, and we keep the one thing Hermes doesn't have and we do: a real security boundary. That combination is something none of Hermes, Paperclip, or today's Relix is alone.*
