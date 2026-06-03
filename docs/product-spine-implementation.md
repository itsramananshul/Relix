# Product Spine - Implementation Map

> **Implementation reference** for the idea-layer docs:
> [`relix-company-model.md`](relix-company-model.md),
> [`relix-execution-and-issue-design.md`](relix-execution-and-issue-design.md),
> [`relix-dashboard-design.md`](relix-dashboard-design.md),
> [`relix-agent-adapters.md`](relix-agent-adapters.md),
> [`relix-hermes-integration.md`](relix-hermes-integration.md), and
> [`relix-lexicon.md`](relix-lexicon.md). This maps the lexicon to the
> shipped code: modules, mesh capabilities, and known divergences.

## Modules

| Module | What |
|---|---|
| `coordinator/agent/store.rs` | Operatives: `reports_to` (Lead), `rig`, `monthly_allowance_cents`, runtime Keys (`max_concurrent_runs`, `wake_on_timer`, `wake_on_demand`); org-tree queries, `manages`, status counts, and the hire flow. |
| `coordinator/spine/` | Mandates, Campaigns, Guilds, Allowance, and the strategy gate. Tenant-scoped store plus `mandate.*`, `campaign.*`, and `guild.*` handlers. |
| `coordinator/brief.rs` | Brief board vocabulary, priorities, reviewer field, and the `BriefCard`, `Dossier`, and `BriefFields` types. |
| `coordinator/mod.rs` (`TaskStore`) | Brief ledger: board moves with guarded side effects, two-pointer Claim fields (`checkout_run_id`, `execution_run_id`, holder, locked-at), lease/heartbeat/release, persistent Brief wakeup queue (`queued`, `running`, `coalesced`, `deferred`, `skipped`, terminal rows), Sub-briefs, Snags, Dossiers, spine fields, board/ready/blocked/stale queries, progress rollups, link listings, and Chronicle events. |
| `coordinator/heartbeat.rs` | Dispatch loop: timer wakes ready Briefs through the persistent wakeup queue, lazily claims queued wakeups with per-Operative concurrency caps, honors timer-wake disablement, runs on Rig, advances board, parks failures, marks wakeup terminal, promotes deferred wakeups, and mints/revokes bridge-back tokens per Shift. |
| `rig/` | Universal agent-backend contract (`Rig` trait), registry, `EchoRig`, `ProcessRig`, CLI Rigs, probe/install hints, structured-output flag, bridge-back support flag, billing metadata, and per-method bridge-back token store. Hermes is currently a stdio `BoxLevel` placeholder, not the real rich Tether. |
| `macros/` | Native Macro / execute-code core: capped execution, interpreter allowlist, split `@relix-call` tool requests, scoped `cwd` and env. |
| `tradecraft/` | Keeper scaffolding: usage-clock Knack aging, provenance gate, creation trigger, post-response nudge. |
| `bench/` | Bench scaffolding: sleep/wake workspace lifecycle and idle hibernation tick. |
| `src/controller_runtime.rs` | Wiring: spine handlers, shared Rig registry, `rig.list`/`rig.describe`, `RELIX_DEFAULT_RIG`, optional live heartbeat loop, prompt composition, failure parking, and bridge-token sweep. |
| `relix-cli` `call.rs` | `relix call --method <name> --arg <pipe-delimited>`: generic operator escape hatch for the spine capability surface. |
| `relix-web-bridge` `spine.rs` | Dashboard HTTP proxy for `/v1/spine/*`, including board reads, Brief create/move/comment/due/pin/set, Mandates, Roster, Desk, search, overdue, Guild detail, and Allowance committed reads. |
| `relix-web-bridge` `bridge_back.rs` | Narrow public bridge-back API for scoped Rig tokens: comment, Sub-brief, Dossier add, Snag set, Clearance request, and claim-holder lookup. Every route validates `Authorization: Bearer brt_*` through `bridge_back.authorize` before forwarding. |
| `relix-web-bridge` `spine_dashboard.html` | Interim `/spine` company console: self-contained HTML/CSS/JS, left rail, Issues board/list, detail properties panel, assignee/reviewer controls, companion chat, Mandate hierarchy/drilldown, Org/Roster, live Allowance summary, and live Chronicle Activity tail. It follows the Paperclip-like work-object IA but is not the final React SPA from `relix-dashboard-design.md`. |
| `relix-web-bridge` `agent.rs` | Roster HTTP API for listing, reading, and patching Operatives, including persisted runtime Keys surfaced to the dashboard. |
| `relix-web-bridge` `companion.rs` | Rule-based materialize-work parser behind `POST /v1/spine/companion`. It creates/moves/searches Briefs and Mandates through the same spine API. Not yet an LLM companion. |

## Capabilities

**Guild** - `guild.get`, `guild.counts`, `guild.set`, `guild.set_allowance`

**Mandate** - `mandate.create/get/list/update`, `mandate.children`, `mandate.tree`, `mandate.search`, `mandate.progress`, `mandate.briefs`, `mandate.propose_strategy/approve_strategy/reject_strategy/strategy`

**Campaign** - `campaign.create/get/list/update`, `campaign.search`, `campaign.progress`, `campaign.briefs`

**Brief** - `brief.create`, `brief.move`, `brief.set`, `brief.fields`, `brief.detail`, `brief.search`, `brief.set_labels`, `brief.labels`, `brief.by_label`, `brief.pin`, `brief.set_due`, `brief.due`, `brief.overdue`, `brief.board`, `brief.board_summary`, `brief.desk`, `brief.workload`, `brief.team_workload`, `brief.subbrief_progress`, `brief.comment`, `brief.ready`, `brief.children_done`, `brief.blocked`, `brief.blocked_list`, `brief.stale_list`, `brief.subbrief`, `brief.unsubbrief`, `brief.subbriefs`, `brief.parents`, `brief.snag`, `brief.unsnag`, `brief.snags`, `brief.blocking`, `brief.dossier_add`, `brief.dossiers`, `brief.dossier_get`, `brief.dossier_latest`, `brief.wakeup`, `brief.wakeups`, `brief.claim`, `brief.heartbeat`, `brief.release`, `brief.claim_holder`

**Operative / Roster** - `agent.create/get/list/update/delete/keys`, `agent.request_hire`, `agent.request_hire_for_mandate`, `agent.approve_hire`, `agent.reject_hire`, `agent.reports`, `agent.branch`, `agent.line`, `agent.peers`, `agent.by_role`, `agent.manages`, `agent.roster_summary`, `agent.allowance_committed`, hire status flow

**Rig** - `rig.list`, `rig.describe` with name, label, governance, bridge-back support, structured-output support, billing metadata, and probe/install hint; per-Operative `rig`; `dispatch_batch` runs a Brief on its Rig

**Bridge-back** - `bridge_back.authorize` plus public HTTP routes under `/v1/bridge-back/*`, including Brief-local Clearance requests

**Chronicle** - `brief.created`, `brief.board_moved`, `brief.assigned`, `brief.reviewer_assigned`, `brief.comment`, `brief.subbrief_added`, `brief.subbrief_removed`, `brief.snagged`, `brief.snag_cleared`, `brief.dossier_added`, `brief.clearance_requested`, `brief.shift_done`, `brief.continued`, `brief.dispatch_failed`

## Governance And Security Carried Through

- Default-deny agent gate; a pending hire is inert.
- Heartbeat Rig resolution now refuses non-active Operatives before dispatch, so pending/suspended/disabled hires cannot run through the default-Rig path.
- Heartbeat wake admission now checks the assigned Operative's `wake_on_timer` flag and `max_concurrent_runs` cap before claiming queued work.
- Tenant-scoped spine reads for Mandates/Campaigns/Guild state.
- Org tree is cycle-guarded.
- Thin Rigs are governed at the box boundary and through scoped bridge-back tokens.
- Bridge-back tokens are scoped per Shift, Brief, Operative, and method; tokens are checked by `bridge_back.authorize`, exposed only through narrow HTTP routes, and revoked when the Shift ends.
- Thin Rigs can raise a real Brief-linked Clearance through `brief.clearance_request`; the handler derives subject/approver metadata from the stored Operative profile, parks the Brief in `awaiting_input`, and writes a pending `approval_requests` row.
- Macro execution is allowlisted and `ProcessRig` stdout is capped.
- Hard dispatch failures park the Brief in `blocked` and Chronicle the reason.
- Strategy gate is queryable and tenant-guarded. The explicit `agent.request_hire_for_mandate` team-build path refuses until the Mandate strategy is approved; legacy/manual `agent.request_hire` and direct `agent.create` remain operator/admin paths and do not check a Mandate.

## Shipped This Roadmap

- Phase 5 materialize-work: rule-based companion plus command-bar operations through `/v1/spine/*`.
- Phase 6 interim dashboard: `/spine` work-object console over the live spine API.
- Execution invariant pass: reviewer guard for `in_review`, assignee/no-Snag guard for `in_progress`, two-pointer Claim fields, lock clearing on assignee/status ownership changes, persistent Brief wakeup queue with queue/coalesce/defer/skipped audit rows, lazy claim stamping, deferred promotion on lock release/status unlock, scoped bridge-back HTTP routes.
- Strategy-gated hire slice: `agent.request_hire_for_mandate` blocks team-build hires until the target Mandate has an approved strategy, then creates only a pending inert hire.
- Bridge-back Clearance slice: `/v1/bridge-back/briefs/:id/clearance` plus `brief.clearance_request` creates a pending approval from a scoped Rig token.
- Runtime Keys slice: Operatives persist timer-wake permission, on-demand wake permission, and max concurrent Brief runs; heartbeat dispatch enforces timer wake and concurrency caps, `brief.wakeup` records skipped rows when wake admission is disabled, and the dashboard can edit those values from the Roster panel.
- Costs panel slice: `/spine` reads `guild.get` and `agent.allowance_committed` through browser-safe spine proxies and shows configured Guild cap, committed Operative allowance, remaining headroom, and over-cap warning. The panel now states the dispatch-level enforcement state (see the Allowance hard-stop slice below); the Guild-cap figures themselves remain visibility only.
- Allowance hard-stop slice (Brief dispatch): the heartbeat dispatcher now refuses to run a Brief when the assigned Operative is over its Allowance. `heartbeat::allowance_admits(monthly_allowance_cents, spend_micros)` is the pure verdict; `dispatch_batch_with_policy` takes an `admit_budget` closure, and the live loop (`controller_runtime.rs`) builds it from the Operative's `monthly_allowance_cents` plus its trailing-30-day spend read from the metrics ledger (`MetricsQuery::cost_since(agent_id, since_ms)`). A refused Brief is parked in `blocked` (not silently skipped), gets a `brief.budget_refused` Chronicle event (`budget_refused: agent_id=… allowance=…c used=…u reason=…`), its wakeup is closed `failed`, and its Claim is released. **Honest limits:** (1) an Operative allowance of `0` is a deterministic hard-stop; a positive cap is **best-effort** — it only counts priced AI calls whose recorded `agent_name` matches the Operative's `agent_id` in the metrics ledger (`cost_micros`), so missing/unattributed spend fails open. (2) The window is a trailing 30 days, not a calendar month with reset bookkeeping. (3) Guild-level Allowance is **not** spend-enforced at dispatch (no tenant-scoped spend query, and Briefs carry no tenant at the dispatch site) — it remains visibility/commitment only.
- Activity slice: `/spine` renders the existing `/v1/tasks/events/recent` Chronicle tail instead of a dead placeholder.
- Goals slice: `/spine` renders the tenant Mandate hierarchy and side-panel drilldown through `mandate.list` and `mandate.tree`.
- Adapter metadata pass: Rig descriptions expose probe status, install hints, structured-output support, bridge-back support, and subscription billing metadata for Claude/Codex/Gemini.
- Macro RPC-to-tools parse layer; Hermes stdio placeholder; Keeper in-memory scaffold.

## Known Divergences From The Design Docs

This is the honest ledger. Do not describe these as done until the code is actually wired and tested.

### Execution And Issue Design

- Board transitions still use a rigid edge graph. `relix-execution-and-issue-design.md` wants permissive target validation plus guarded side effects. Guarded side effects now exist; the transition validator is still stricter than the design.
- Claim has the two-pointer fields now, but it is not the full Paperclip checkout engine. Implemented: `checkout_run_id`, `execution_run_id`, holder, locked-at, self-refresh, lease/release/admin force-release, lock clearing, persistent wakeup rows, queue/coalesce/defer/skipped decisions, lazy lock stamping from queued wakeups, and deferred promotion. Missing: stale-run adoption by terminal run evidence, explicit HTTP 409 semantics, per-agent start lock, and a dedicated queued-run table separate from wakeup rows.
- Entry guards are enforced for the current board path: `in_progress` requires an assignee and no unresolved Snags; `in_review` requires `reviewer_agent_id`.
- There is now a Brief-local wakeup queue and coalesce/defer/queue engine for the heartbeat dispatcher. Implemented: timer wake admission, lazy claim stamping, deferred promotion, per-Operative max-running caps on queue claims, and a per-Operative Allowance hard-stop admission gate at dispatch (see the Allowance hard-stop slice above; refused Briefs park in `blocked` with a `brief.budget_refused` event). Still missing: all external triggers funneled exclusively through that chokepoint, a true per-agent start lock separate from claim counts, a Guild-level spend gate, and the conservative recovery chain. Failure handling is otherwise still simple failure-to-blocked parking.
- Exactly-once plan decomposition, issue documents as approval-bound Dossiers, structured interaction cards, cost tree rollup, and child/blocker auto-wake promotion remain incomplete.

### Company Model

- Strategy gate is enforced only on the explicit `agent.request_hire_for_mandate` path. The broader Prime/CEO flow is still incomplete: direct/admin `agent.create`, legacy/manual `agent.request_hire`, and any future team-build helpers must choose this guarded path or add equivalent enforcement.
- Per-agent Keys are partial: `rig`, `monthly_allowance_cents`, `max_concurrent_runs`, `wake_on_timer`, and `wake_on_demand` are stored and editable from the dashboard. Heartbeat dispatch enforces `wake_on_timer`, `max_concurrent_runs`, and the per-Operative Allowance hard-stop (see below); the `brief.wakeup` chokepoint enforces `wake_on_timer`/`wake_on_demand` for explicit queued wakes. Still missing: spawn-routing, assign-scope, secrets allowlists, and broader autonomy policy gates. Any future wake trigger must use `brief.wakeup` or it will bypass the on-demand policy.
- Operative Allowance is now **enforced at Brief dispatch** (not just a stored cap): an Operative with `monthly_allowance_cents = 0` is hard-stopped, and a positive cap stops dispatch once its trailing-30-day recorded AI spend reaches the cap (best-effort — see the Allowance hard-stop slice for the exact limits). Still incomplete: a Guild-level spend hard-stop, calendar-month windows with reset bookkeeping, an issue-tree cost rollup, and billing-code attribution. The positive-cap gate is only as accurate as the metrics ledger's per-Operative spend attribution.
- The Prime/CEO flow is not real yet: there is no governed strategy-to-team assembly loop.

### Adapters And Hermes

- Tether / plugin-hook system is unbuilt. The existing plugin host is an out-of-process capability-provider system, not the in-process lifecycle-hook bridge described in `relix-hermes-integration.md`.
- Hermes rich seam is unbuilt. `hermes_rig` is a stdio placeholder and remains `BoxLevel`; real `/v1/runs`, MCP, `relix-bridge`, and PerToolCall governance are future work.
- CLI adapters declare structured-output flags and metadata, but do not parse stream-json/JSONL yet. Missing: token/cost capture, `$0-but-tracked` cost ledger rows, quota polling/back-off, Codex per-tenant `CODEX_HOME` symlink, session resume, interrupt, and stop.
- Bridge-back exists for the narrow universal floor: comment/Sub-brief/Dossier/Snag/Clearance/claim-holder routes. Thin adapters still have no per-tool-call governance; Clearance is the route for an adapter to ask Relix before crossing a boundary it cannot gate internally.

### Dashboard

- The current dashboard is an interim single-file shell, not the decided React SPA in `relix-dashboard-design.md`.
- The page now follows the work-object IA, but it lacks a componentized router, query cache, tenant-prefix navigation, and surgical realtime invalidation.
- Missing or partial surfaces: true Desk/Inbox, pan/zoom Lattice org chart, complete governance panel for spawn/assign/secrets policies, full Costs/Approvals with spend enforcement controls, issue-as-live-chat-thread detail with transcripts/interactions, Settings hub, and websocket-driven realtime.

## Remaining External-Infra Work

- Smarter companion: replace the rule parser with an LLM that uses the same governed spine APIs.
- Sandboxed Cell: container/VM isolation before exposing powerful Macro/Rig execution broadly.
- Persistent Keeper and Bench backends behind the in-memory ledgers.
