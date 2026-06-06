# Relix Dashboard — Deep Design

> **Companion to `relix-company-model.md` and `relix-execution-and-issue-design.md`.** Ideas and structure only — no code. Grounded in a complete read of Paperclip's React dashboard (all 171 components + 61 pages + the realtime layer) and Relix's current operator console (the single-file 22-panel `dashboard.html` served by the web-bridge).
>
> **The goal:** reshape Relix's operator UI from a *grid of feature panels* into a *goal-facing company console* — organized around work objects, with the issue board as the spine and the issue as a live chat thread — keeping every existing Relix capability, just relocated to where it belongs.

---

## 0. The starting point (Relix today vs the target)

> **STATUS (Phase 2 complete):** the React SPA in `apps/dashboard` is now the canonical and ONLY dashboard, served at `/dashboard`. The original self-contained `dashboard.html` console described below has been **deleted** (Phase 2 Slice 3); the interim `/spine` board was deleted in Slice 2 (`/spine` now 308-redirects to `/dashboard`). The text below is kept as the design rationale for that migration — it is historical context, not the current state.

**Relix originally (the starting point this doc reshaped):** one self-contained `dashboard.html` (CSS + ~8K lines of inline JS, no build step) served by the web-bridge, with **22 flat panels** in five groups (Overview, Tasks, Cron, Chat, Memory, Skills, Sessions, Reasoning, Approvals, Credentials, Identity, Cost, Observability, Denials, Tenant, Planning, Workflows, Email, Plugins, MCP, Config, Logs). It already had a Paperclip-inspired grouped nav and spine-status badges — a real head start — but it was organized **by feature**, so it felt like a control panel.

**The target:** a console organized **by work object** — Inbox · Issues (board) · Projects · Goals · Org · Agents · Approvals · Costs · Activity · Chat — where the 22 feature panels become *detail tabs on the object they describe* (memory on an agent, confidence on a run, etc.). Same power, goal-facing shape.

---

## 1. Decision: the front-end stack

This is the one upfront decision the reshape forces. The target experience — a drag/drop kanban board, an issue rendered as a streaming chat thread, a pan/zoom org chart, surgical realtime updates — is genuinely hard to build well in one hand-written HTML file. Paperclip uses a React + Vite SPA (TanStack Query for data, a custom company-prefix router, an agent-chat runtime) precisely because this class of UI needs it.

**DECIDED (now DONE): build a real React SPA. The vanilla `dashboard.html` is retired — not grown.** (React + a query cache + a component library), served by the web-bridge the same way the HTML file was, so deployment doesn't change. The 22-panel vanilla console was thrown away, not preserved — it stayed reachable only until the SPA reached parity, and was **deleted in Phase 2 Slice 3**. Paperclip's React app is the proven structural blueprint to mirror (not copy visually — see §12 for the look).

---

## 2. The shell (three zones)

Mirror Paperclip's proven shell:

- **Left nav rail** (collapsible, resizable, persisted width; collapses to icons). Swaps by context: the main company nav, an **Instance/Settings** nav under settings routes, and a **Company-settings** nav under company-settings routes.
- **Main content** (full-width) with a **breadcrumb bar** on top.
- **Right "properties" panel** (contextual, ~320px) that pages inject content into for the selected entity — *not* a fixed sidebar; it appears on detail views (issue properties, goal properties, agent config) and slides away elsewhere.
- **Mobile:** the left nav becomes an off-canvas drawer (edge-swipe to open), plus a fixed **bottom nav** (Home / Issues / **Create** / Agents / Inbox) — so the whole thing is usable from a phone, which matters for "monitor my autonomous company on the go."

Global singletons mounted once in the shell: the command palette (⌘K), the New-Issue / New-Agent / New-Project / New-Goal dialogs, a toast viewport, and the keyboard-shortcuts cheatsheet.

---

## 3. Navigation (grouped around work objects) + where the 22 panels go

**Top of the rail:** a **tenant/company switcher** (Relix is multi-tenant — this is the "which company am I running" selector), a global **search** button, and a **New Issue** button.

**Personal section:** **Inbox** (badge = pending approvals + budget alerts + failed runs + stranded work) and **My Issues**.

**Work section:** **Issues** (the board — the workhorse), **Routines**, **Goals**.

**Company section:** **Dashboard** (the glance view), **Org** (the chart), **Agents**, **Costs**, **Activity**, **Chat** (the companion), **Settings**.

**Where the current 22 Relix panels relocate** (nothing is lost — everything gets a home on the object it describes):

| Current Relix panel | New home |
|---|---|
| Tasks | becomes **Issues** (the board) — the spine |
| Cron | **Routines** |
| Chat | the **Chat companion** (upgraded — see Part 13) |
| Overview | the **Dashboard** glance view |
| Memory | a tab on the **Agent** detail (its memory) + a company-level knowledge view |
| Skills | a tab on the **Agent** detail (and a company Skills library under Settings) |
| Sessions | a tab on the **Agent** detail / surfaced per **Run** |
| Reasoning / Confidence / Judge / Belief | tabs on a **Run** detail ("how sure / how it decided") |
| Approvals | the **Approvals** surface + the **Inbox** |
| Credentials / Secrets | **Settings → Secrets** + per-agent "secrets it may use" toggles |
| Identity | **Settings → Access/Identity** + the agent's identity on its detail |
| Cost | **Costs** (with budgets) + per-issue/agent/project cost |
| Observability / Metrics | **Costs/Activity** or a **System** area; per-agent health on the agent |
| Denials (policy) | **Activity** (governance) + **Settings → Policy** |
| Tenant | the tenant switcher + **Settings → Multi-tenant** |
| Planning | folded into **Issues** (the plan-as-document + decomposition flow) |
| Workflows | **Routines** / the orchestration view; or kept under **System** |
| Email / Channels | **Settings → Channels** + channel status tiles |
| Plugins / MCP | **Settings → Plugins / MCP** + per-agent tool toggles |
| Config | **Settings → Configuration** |
| Logs | **Settings → System → Logs** (and per-run transcript inline) |

The rule: **a feature lives on the object it acts on.** An agent's memory/skills/sessions show on the agent. A run's confidence/reasoning shows on the run. Governance (policy/secrets/tenants/plugins) lives under Settings. The top-level nav is *only* work objects.

---

## 4. The tenant/company-prefix router (steal this exactly)

Paperclip's cleverest structural trick: it re-exports the router but **overrides the navigation primitives** so any link written as `/issues` is automatically rewritten to `/{companyPrefix}/issues`. Every page is written company-agnostic; the prefix is woven in transparently. URLs are shareable and company-scoped, but pages never think about the prefix.

For Relix (multi-tenant mesh), the same idea applies with a **tenant prefix**. The selected tenant lives in app state (not forced into every URL the developer writes), and the router injects it. This one pattern is what keeps the whole app clean as it grows — adopt it on day one of the SPA.

---

## 5. The Inbox (the operator's home)

The single action center showing only **what needs you**, in priority order, computed from live state (not a notification table):

1. **Approvals** (hire / strategy / budget-override / high-risk) — inline Approve/Reject.
2. **Recovery decision cards** (the diagnosis-driven escalations — see `relix-execution-and-issue-design.md` §3.3b): when work can't self-heal, a card explains the root cause *in plain language* and offers one-click choices — **Retry now / Block / Reassign / Investigate (opens the chat companion) / Dismiss**. Transient failures retry silently and never appear here, so the Inbox stays signal, not noise.
3. **Alerts** (agent errors, budget thresholds crossed, failed runs).
4. **Stranded / blocked work** (issues stuck with nobody moving them — surfaced by the recovery layer's liveness signals).

This is where the Board (you) lives day-to-day. The sidebar Inbox badge is the sum of these.

---

## 6. The Issue board (the spine)

One surface owns list↔board, and it's the most-used screen. Mirror Paperclip's `IssuesList` design:

- **List ↔ Board toggle.** Board = kanban; list = grouped rows.
- **Columns** = the issue statuses (Backlog → Todo → In Progress → In Review → Blocked → Done → Cancelled). Drag a card to a column → status mutation (with transition validation; an invalid drop shows a toast).
- **The kanban is "dumb" and density-driven** — the list container owns the brains. Above a high-volume threshold it auto-switches to compact cards and collapses cold lanes (backlog/done/cancelled), with per-column "show more" pagination so columns aren't truncated.
- **Grouping** (list mode): by status / priority / assignee / project / parent / none. **Sub-issue nesting** (a tree, indented).
- **The "workflow checklist" rendering** (the goal-facing magic): when sorted by workflow with nesting, issues render as a numbered plan (`1`, `1.1`, …) with inline "blocked by X · step N" chips — a tree of work reads like a goal-facing checklist. This is a big part of *why* Paperclip feels organized; copy it.
- **A progress strip** on a parent: done/in-progress/blocked counts, a segmented bar, "next up / waiting on blockers," and live cost (tokens + spend) for the subtree.

Cards show: identifier (mono), title, priority icon, assignee avatar, a pulsing **"Live"** dot when an agent is running it, and a "Next step" chip when a successful run needs a disposition.

---

## 7. The Issue detail (the heart)

This is where the product lives. Three-pane: **description + conversation** in the middle, **properties** on the right, with sections below. Mirror Paperclip's `IssueDetail` + `IssueChatThread`:

- **Header:** status (clickable to change), priority, identifier, live pill, project/goal links, labels, parent-chain breadcrumb, a pause-hold banner if the subtree is held, action buttons (assign / copy-as-markdown / properties toggle).
- **Title + description:** inline-editable, with @-mentions and image upload.
- **The conversation (the centerpiece):** the issue *is* a chat thread. It merges, in one stable timeline: your comments, agent messages, **live run transcripts** (the agent's tool calls/thinking/output streaming in), system notes (status/assignee changes, run summaries), and **interaction cards**. Built on an agent-chat runtime so streaming feels native. Relix already renders run transcripts — this reuses that rendering inside the thread.
- **Interaction cards** (answerable inline): **Ask questions** (radio/checkbox), **Request confirmation** (yes/no — used for plan approval), **Suggest tasks** (a selectable sub-task tree the agent proposes → you accept and they're created/assigned). A proposed child that carries an **assignee hint** (an Operative id or a role) shows that hint as a chip **before** you accept (so you see who each child would be assigned to — still validated through the assign-Key gate on accept); after accept, each created child shows its resolved state — **assigned: <who>** or **needs assignment** (deep-linked to its board card). This is what turns "the agent asks" from a dead-end comment into a click.
- **The composer:** markdown + mentions + attachments, a **work-mode toggle** (standard / planning), and reopen/interrupt semantics (comment to reopen a done issue, or interrupt a running agent).
- **Sections below:** sub-issues (with progress), documents (plan/design with revisions + the plan-approval flow), blockers, run history (the ledger), work products (deliverables/PRs/preview URLs), attachments.
- **Properties panel (right):** status, priority, assignee, project, goal, labels, blockers (add/remove), parent, reviewers/approvers, the model lane (which model/effort), monitor, and the workspace (branch/folder/service URL).

---

## 8. The Run transcript view

A standalone, reusable renderer (used in the issue thread, on the agent's Runs tab, and in the live-runs view). Mirror Paperclip's block-grouping: fold the raw adapter stream into typed blocks — assistant/thinking message, tool-call cards (matched call→result, status running/done/error), grouped consecutive tools/commands into collapsible accordions, batched stderr/system logs, colorized diffs, and init/result events (with tokens + cost). Two modes — a "nice" grouped view (default) and a "raw" virtualized dump. Live-tailed via the realtime socket + polling fallback. Relix already produces run transcripts; this is the *rendering* pattern to adopt.

---

## 9. The Org chart + the per-agent permission panel (the governance surface)

The **org chart** is a pan/zoom/pinch tree (SVG edges + cards), each node showing the agent's icon, a live status dot (running/idle/paused/error), role/title, and adapter. Clicking a node opens the agent detail — which is *also* where you govern it.

The **agent detail** has tabs: **Overview** (latest run, charts, recent issues, cost), **Instructions** (its markdown bundle — the job description, editable), **Skills**, **Configuration** (adapter/model), **Runs** (the transcript master/detail), **Budget**, and — the one you care most about — **Permissions**.

**The Permissions panel is where Relix deliberately goes denser than Paperclip** (which ships only two toggles). It's a clean, grouped switchboard (main doc §5.2):
- **Org powers:** can spawn/hire agents (+ *directly* vs *route hires through its boss*), can configure other agents (scoped to its subtree), can manage others' work.
- **Work powers:** can assign/delegate work, with a **scope** (anyone / only my subtree / specific agents / specific projects).
- **Capability powers:** tools it may use (per-tool/category toggles), secrets it may access, risk ceiling, actions that always require approval.
- **Autonomy & budget:** scheduled heartbeat on/off + interval, wake-on-assignment, concurrency, monthly budget.

Because Relix's existing agent-gate already understands categories, risk, secrets, and scopes natively, this panel is a *face* on machinery that exists — not new enforcement logic. Consider shipping **role presets** (CEO / manager / worker / read-only) that set sensible toggle bundles, with the raw toggles underneath for power users.

---

## 10. The other surfaces (briefly)

- **Goals:** a goal tree; each goal shows sub-goals + linked projects; the "why" hierarchy.
- **Projects:** workstreams; a project detail has Issues / Overview / Configuration / Budget tabs.
- **Costs:** spend by company/agent/project/issue, budget progress bars, incident cards (resolve = raise-and-resume / keep-paused), provider/biller breakdowns. The issue-tree cost rollup (a planner's issue shows the whole effort's cost) surfaces here and on the issue.
- **Approvals:** the gate list (pending/all) + detail with the typed payload (hire / strategy / budget / high-risk) and Approve/Reject/Request-revision.
- **Agents:** the employee list (status filters) + the org-view toggle.
- **Activity:** the audit/event stream (Relix's hash-chained audit, product-faced).

---

## 11. Realtime (one socket → surgical updates)

Mirror Paperclip's realtime engine, which Relix is well-positioned for because it **already has a per-tenant live-events WebSocket**. The pattern:
- Open **one socket per selected tenant**.
- Incoming events (`run.status`, `agent.status`, `activity.logged`, etc.) drive **surgical cache invalidation** keyed by a centralized, hierarchical **query-key factory** — not blanket refetches.
- **Route-aware optimizations:** if you're *viewing* the affected issue, hydrate the new comment directly into the cache (no flicker); use "inactive" refetch for background queries.
- **Rate-limited toasts** (e.g. max 3 per category per 10s), and suppress toasts for the issue you're already looking at.

This is what makes the console feel alive without thrashing. The query-key factory discipline is the unglamorous thing that makes it tractable — adopt it early.

---

## 12. The design system / feel

- **Aesthetic: Vercel-style black & white — stark, clean, high-contrast, minimal.** True-black / true-white base, generous whitespace, thin low-contrast gray borders, flat surfaces (almost no shadows), tight geometric corners. A clean sans-serif (Geist / Inter family) for text and a **monospace (Geist Mono) for identifiers, numbers, and code**. Both light and dark modes share the same stark B&W base. **Color is reserved for meaning only** (status / priority) and even then kept muted and minimal — never decorative. This is the look: clean, confident, monochrome — *not* Paperclip's charcoal-and-blue Linear style (we copy Paperclip's structure, not its skin).
- **Dense but scannable, keyboard-first** (⌘K palette, `c` = new issue), built on a small primitives library (buttons/badges/cards/dialogs/popovers/tabs/etc.) over CSS-variable tokens.
- **Status/priority vocabulary stays, rendered restrained:** one consistent set (blocked, live/running, done/healthy, error) but as muted dots/badges that fit the B&W minimalism — never loud color fills.
- **Progressive disclosure:** human summary → steps/artifacts → raw transcript. **No log-worship** — raw logs are one click deeper, never the default.
- **No silent failures:** every failed run is visible (this aligns with Relix's existing "honesty contract" — surface problems, don't hide them).
- **Skeletons not spinners; optimistic edits with rollback; placeholder-data to avoid layout jumps.**

---

## 13. The chat companion (deep design)

This is the surface you described — and it's the bridge between "reason with the model" and "issue-first." It is *not* a generic chatbot; it's a **context-aware operating console for the company**.

### What it is
- **Context-aware.** The companion can read the live company state — current issues, the org, who's running, recent activity, costs, budgets. Ask "what's the planner stuck on?" and it actually knows, because it has read access to the same data the dashboard shows.
- **A thinking partner.** You talk through what you're trying to do; it reasons back, proposes options, points out tradeoffs (cost, risk, who should own it).
- **A materializer.** When you like a direction, you say it in plain language and it **creates real, governed work objects**: *"make this an issue and assign it to the CTO," "have the CEO spin up a research team for this," "ship it / put it in production," "raise the marketing budget to $50."*

### How it's built (reusing what Relix has)
The companion is essentially a **special operator-facing agent** with two things wired in:
1. **Read tools over the company state** — list issues/agents/goals/runs/costs (the same API the dashboard uses).
2. **Write tools that create work objects** — create an issue, assign it, create a goal/project, instruct the CEO (i.e. hand the CEO an issue), set a budget, etc.

Relix already has the substrate for this: the OpenAI-shim chat surface, an AI node that can already read memory/state, the tool/dispatch layer, and the permission gate. The companion is that chat surface upgraded with company-state tools and work-creation tools — *not* a new engine.

### Governance (critical)
- **It runs as you (the Board), through the same gates.** Anything the companion creates — an issue, a hire request, a budget change — passes the *same* permission and approval gates as if you'd clicked the buttons. The chat does not bypass governance.
- **Preview-then-confirm for anything that spends money or hires.** For low-risk acts (create an issue, add a comment) it can act directly; for spend/hire/destructive acts it shows a **preview** ("I'll create these 3 issues and assign them to CTO / hire a planner agent — confirm?") and waits for your click. This keeps the conversational speed without surprise side-effects. *(This is one of the main-doc open questions — recommendation: preview-then-confirm for spend/hire/destructive.)*
- **Everything it does is audited.** Because it acts through the normal API + admission pipeline, every action lands in the activity log with the companion as the actor — full traceability.

### Where it lives in the UI
A top-level **Chat** entry (and reachable from anywhere via ⌘K). It can also be **contextual**: open it from an issue ("ask the companion about this issue") and it's pre-loaded with that issue's context. The result of a conversation is usually a link to the issue(s) it just created — so the chat *hands off to the board*, reinforcing "reason in chat, work lands as issues."

### The OpenAI-compatibility side effect
Because the chat surface stays, Relix keeps working as an OpenAI-compatible endpoint for external clients — but the *primary* chat becomes this company-aware companion, not a bare passthrough.

---

## 14. Build order for the dashboard (Phase 6, but seeded earlier)

The dashboard reshape is roadmap Phase 6, but pieces are needed as soon as their objects exist. A buildable order:

1. **SPA shell + tenant-prefix router + realtime wiring** (the foundation; reuses the existing live-events socket).
2. **The Issues board + issue detail (with the chat thread)** — the spine; lands with Phase 1's Issue object.
3. **The Inbox + Approvals** — lands with Phase 2's governance.
4. **The Org chart + the per-agent permission panel** — lands with Phase 2's org model.
5. **Goals / Projects / Costs / Activity** surfaces.
6. **The chat companion** — lands with Phase 4.
7. **Relocate the 22 feature panels** into their object tabs / Settings, retiring the legacy HTML as each lands.

The migration is safe because the web-bridge keeps serving one bundle; the legacy panels stay reachable until each replacement ships.

---

*This is the dashboard-and-companion design. With the company model (`relix-company-model.md`), the execution spine (`relix-execution-and-issue-design.md`), and this, the three docs cover the product, the engine, and the surface — all grounded in the complete Paperclip read, all ideas-only. The next concrete step is to pick the first build slice (Phase 0/1) and design its exact data shape against Relix's coordinator schema.*
