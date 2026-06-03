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
| `coordinator/agent/store.rs` | Operatives: `reports_to` (Lead), `rig`, `monthly_allowance_cents`, runtime Keys (`max_concurrent_runs`, `wake_on_timer`, `wake_on_demand`), and the org/work Keys (`can_spawn_agents`, `spawn_route`, `can_assign_work`, `assign_scope`, `assign_allowed_agents`, `can_manage_work`, `can_configure_agents`, `configure_scope`, `secret_allowlist`, `instruction_bundle`); org-tree queries, `manages`, status counts, and the hire flow. All Keys are validated edits via `agent.update`. |
| `coordinator/agent/keys.rs` | Pure, I/O-free Keys policy: `spawn_verdict` / `assign_verdict` → three-valued `KeyVerdict` (Allow / Clearance / Deny). Unknown scope/route values normalise to the safest option so a garbage value can never widen authority. Exhaustively unit-tested. |
| `coordinator/agent/handlers.rs` | Agent capability handlers + the actor-aware enforcement helpers (`caller_is_operator`, `enforce_spawn_key`, `enforce_assign_key`) and `agent.assign_check`. Operator/admin (Founder/Board) bypasses; an agent actor is gated by its Keys. |
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
| `relix-web-bridge` `spine.rs` | Dashboard HTTP proxy for `/v1/spine/*`, including board reads, Brief create/move/comment/due/pin/set, Mandates, Roster, Desk, search, overdue, Guild detail, and Allowance committed reads. Also exposes composite read payloads that fan out to existing capabilities only: `/v1/spine/inbox` (blocked + stale + overdue + in-review + unassigned in one bounded response), `/v1/spine/unassigned`, `/v1/spine/briefs/:id/events` (a Brief Chronicle as an array), `/v1/spine/briefs/:id/thread` (detail + Chronicle + wakeups + claim holder), `/v1/spine/keys/:agent` (the full Operative profile for the Keys panel), `/v1/spine/assign_check` (assign-Key preview), and `/v1/spine/clearances` (pending Clearances parsed from `coord.approval.pending`'s TSV). |
| `relix-web-bridge` `bridge_back.rs` | Narrow public bridge-back API for scoped Rig tokens: comment, Sub-brief, Dossier add, Snag set, Clearance request, and claim-holder lookup. Every route validates `Authorization: Bearer brt_*` through `bridge_back.authorize` before forwarding. |
| `relix-web-bridge` `spine_dashboard.html` | Interim `/spine` company console: self-contained HTML/CSS/JS, left rail, Issues board/list, a real Desk/Inbox (sectioned Blocked/Stale/Overdue/In-review/Unassigned from `/v1/spine/inbox`), a Brief live work thread (claim holder, status/priority/assignee/reviewer editors, runs, Blockers, Sub-issues, Chronicle timeline, comment) from `/v1/spine/briefs/:id/thread`, an Operative Desk (their active Briefs + allowance/Rig summary) from `/v1/spine/desk/:agent`, a per-Operative **Keys panel** split into "enforced" (spawn + assign Keys) and "stored only" (manage/configure/secrets/charter), pending **Clearances** on the Desk (read-only, with an honest "decide via CLI" note), companion chat, Mandate hierarchy/drilldown, Org/Roster, live Allowance summary, and live Chronicle Activity tail. It follows the Paperclip-like work-object IA but is not the final React SPA from `relix-dashboard-design.md`. |
| `relix-web-bridge` `agent.rs` | Roster HTTP API for listing, reading, and patching Operatives, including the runtime Keys and the org/work Keys (`can_spawn_agents`, `spawn_route`, `can_assign_work`, `assign_scope`, `assign_allowed_agents`, `can_manage_work`, `can_configure_agents`, `configure_scope`, `secret_allowlist`, `instruction_bundle`) surfaced to / edited from the dashboard. |
| `relix-web-bridge` `companion.rs` | Rule-based materialize-work parser behind `POST /v1/spine/companion`. It creates/moves/searches Briefs and Mandates through the same spine API. Not yet an LLM companion. |

## Capabilities

**Guild** - `guild.get`, `guild.counts`, `guild.set`, `guild.set_allowance`

**Mandate** - `mandate.create/get/list/update`, `mandate.children`, `mandate.tree`, `mandate.search`, `mandate.progress`, `mandate.briefs`, `mandate.propose_strategy/approve_strategy/reject_strategy/strategy`

**Campaign** - `campaign.create/get/list/update`, `campaign.search`, `campaign.progress`, `campaign.briefs`

**Brief** - `brief.create`, `brief.move`, `brief.set`, `brief.fields`, `brief.detail`, `brief.search`, `brief.set_labels`, `brief.labels`, `brief.by_label`, `brief.pin`, `brief.set_due`, `brief.due`, `brief.overdue`, `brief.board`, `brief.board_summary`, `brief.desk`, `brief.workload`, `brief.team_workload`, `brief.subbrief_progress`, `brief.comment`, `brief.ready`, `brief.children_done`, `brief.blocked`, `brief.blocked_list`, `brief.stale_list`, `brief.subbrief`, `brief.unsubbrief`, `brief.subbriefs`, `brief.parents`, `brief.snag`, `brief.unsnag`, `brief.snags`, `brief.blocking`, `brief.dossier_add`, `brief.dossiers`, `brief.dossier_get`, `brief.dossier_latest`, `brief.wakeup`, `brief.wakeups`, `brief.claim`, `brief.heartbeat`, `brief.release`, `brief.claim_holder`

**Operative / Roster** - `agent.create/get/list/update/delete/keys`, `agent.request_hire`, `agent.request_hire_for_mandate`, `agent.approve_hire`, `agent.reject_hire`, `agent.reports`, `agent.branch`, `agent.line`, `agent.peers`, `agent.by_role`, `agent.manages`, `agent.assign_check`, `agent.roster_summary`, `agent.allowance_committed`, hire status flow. `agent.create` is operator-only; `agent.request_hire`/`agent.request_hire_for_mandate` enforce the spawn Key for agent actors. `agent.update` edits all stored Keys (validated).

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
- Strategy gate is queryable and tenant-guarded. The explicit `agent.request_hire_for_mandate` team-build path refuses until the Mandate strategy is approved; it now *also* applies the spawn Key. `agent.create` is operator-only (an agent actor is refused). Legacy/manual `agent.request_hire` does not check a Mandate but does enforce the spawn Key.
- Spawn Key (company-model §5.2A) is **enforced**: an agent actor calling `agent.request_hire` / `agent.request_hire_for_mandate` is allowed only with `can_spawn_agents`; `spawn_route=lead/founder` surfaces a `clearance:` note and the hire is always pending-inert (never live). The Founder/Board (operator/admin) bypasses. An actor with no Operative profile in the Guild is security-denied. `agent.create` (live creation) is operator-only.
- Assign Key (company-model §5.2B/§5.3) is **enforced** at `brief.set` (field=assignee): an agent actor may assign only within its `assign_scope` (any / branch — resolved live from the org tree / specific — `assign_allowed_agents`); out-of-scope assignment is policy-denied. Clearing an assignee and the Founder/Board path are never blocked. `agent.assign_check` exposes the same verdict read-only.

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
- Desk/Inbox + live-thread slice: a new `brief.unassigned` capability lists active Briefs with no Operative; the bridge composes `/v1/spine/inbox` (blocked + stale + overdue + in-review + unassigned), `/v1/spine/briefs/:id/thread` (detail + Chronicle + wakeups + claim holder), `/v1/spine/briefs/:id/events`, and `/v1/spine/unassigned` from existing capabilities only (no new runtime queries, no fabricated data; all bounded by limit). `/spine` now renders the Inbox as a sectioned Desk, the Brief detail as a live work thread (claim holder + Chronicle timeline), and an Operative Desk (their active Briefs + allowance/Rig summary). **Honest limits:** the page is still the interim single-file shell with no realtime/websocket — the thread refreshes only on open/comment, not via push.
- Adapter metadata pass: Rig descriptions expose probe status, install hints, structured-output support, bridge-back support, and subscription billing metadata for Claude/Codex/Gemini.
- Operative Keys + governance slice (company-model §5.2): Operatives gained the product-level org/work Keys (`can_spawn_agents`, `spawn_route`, `can_assign_work`, `assign_scope`, `assign_allowed_agents`, `can_manage_work`, `can_configure_agents`, `configure_scope`, `secret_allowlist`, `instruction_bundle`), all default-deny and validated. Pure policy helpers (`agent/keys.rs`) decide spawn/assign. **Enforced:** the spawn Key on agent-originated hires (`agent.request_hire` / `..._for_mandate`), `agent.create` made operator-only, and the assign Key at `brief.set` (assignee) using live Branch membership. `agent.assign_check` exposes the assign verdict; the dashboard gained a Keys panel (enforced vs stored-only) and a pending-Clearances surface on the Desk (`/v1/spine/clearances`, read-only). **Stored/displayed only this pass:** `can_manage_work`, `can_configure_agents` + `configure_scope`, `secret_allowlist`, `instruction_bundle`.
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

- Spawn governance now has two independent gates: the strategy gate (only on `agent.request_hire_for_mandate`) and the spawn Key (on both `agent.request_hire` and `..._for_mandate`). `agent.create` is operator-only. **Remaining escape hatches / gaps:** the operator/admin paths are deliberately ungated (the Founder/Board is sovereign); `spawn_route` is recorded and surfaced as a `clearance:` note but all agent-originated hires are pending-inert regardless of route — full route-differentiated auto-approval routing (a real Clearance object the Lead/Founder greenlights to go active) is future work.
- Per-agent Keys: `rig`, `monthly_allowance_cents`, `max_concurrent_runs`, `wake_on_timer`, `wake_on_demand`, and the org/work Keys (`can_spawn_agents`, `spawn_route`, `can_assign_work`, `assign_scope`, `assign_allowed_agents`, `can_manage_work`, `can_configure_agents`, `configure_scope`, `secret_allowlist`, `instruction_bundle`) are stored, validated, and editable from the dashboard. **Enforced:** `wake_on_timer`, `max_concurrent_runs`, the Allowance hard-stop (below), `wake_on_timer`/`wake_on_demand` at the `brief.wakeup` chokepoint, the **spawn Key** (agent-originated hires), and the **assign Key** at `brief.set` (assignee). **Stored/displayed only (NOT yet enforced):** `can_manage_work`, `can_configure_agents` + `configure_scope`, `secret_allowlist` (the vault has no per-Operative read at injection time), and `instruction_bundle` (context only; never executed by the gate). **Assignment-enforcement gaps:** only `brief.set` (assignee) runs the assign gate — `brief.create`'s initial assignee and `brief.move` do not yet; and the `brief.set` gate is skipped when no agent store is wired into `coordinator::register`. Any future wake trigger must use `brief.wakeup` or it bypasses the on-demand policy.
- Operative Allowance is now **enforced at Brief dispatch** (not just a stored cap): an Operative with `monthly_allowance_cents = 0` is hard-stopped, and a positive cap stops dispatch once its trailing-30-day recorded AI spend reaches the cap (best-effort — see the Allowance hard-stop slice for the exact limits). Still incomplete: a Guild-level spend hard-stop, calendar-month windows with reset bookkeeping, an issue-tree cost rollup, and billing-code attribution. The positive-cap gate is only as accurate as the metrics ledger's per-Operative spend attribution.
- The Prime/CEO flow is partially scaffolded: the strategy gate, the spawn Key, and the assign Key are the governance primitives a Prime team-build would ride on, and a Prime can now be expressed as an Operative with `can_spawn_agents` + `can_assign_work` + a charter (`instruction_bundle`). Still missing the autonomous loop that *uses* them: there is no governed strategy-to-team assembly driver that proposes a plan, gets it greenlit, and stands up + assigns a team on its own.

### Adapters And Hermes

- Tether / plugin-hook system is unbuilt. The existing plugin host is an out-of-process capability-provider system, not the in-process lifecycle-hook bridge described in `relix-hermes-integration.md`.
- Hermes rich seam is unbuilt. `hermes_rig` is a stdio placeholder and remains `BoxLevel`; real `/v1/runs`, MCP, `relix-bridge`, and PerToolCall governance are future work.
- CLI adapters declare structured-output flags and metadata, but do not parse stream-json/JSONL yet. Missing: token/cost capture, `$0-but-tracked` cost ledger rows, quota polling/back-off, Codex per-tenant `CODEX_HOME` symlink, session resume, interrupt, and stop.
- Bridge-back exists for the narrow universal floor: comment/Sub-brief/Dossier/Snag/Clearance/claim-holder routes. Thin adapters still have no per-tool-call governance; Clearance is the route for an adapter to ask Relix before crossing a boundary it cannot gate internally.

### Dashboard

- The current dashboard is an interim single-file shell, not the decided React SPA in `relix-dashboard-design.md`.
- The page now follows the work-object IA, but it lacks a componentized router, query cache, tenant-prefix navigation, and surgical realtime invalidation.
- The Desk/Inbox and the Brief live work thread now exist (sectioned needs-attention Desk, claim holder, Chronicle timeline, Operative Desk), but only as request/response reads in the interim shell — there is no websocket-driven realtime, transcript/interaction streaming, or push invalidation; the thread refreshes on open/comment.
- The per-Operative Keys panel now exists (enforced spawn/assign Keys + stored-only manage/configure/secrets/charter), and pending Clearances render on the Desk — but read-only: Clearance approve/reject is not proxied (the bridge does not yet forward an authorised-approver identity to `coord.approval.decide`), so greenlighting is still via the operator CLI.
- Missing or partial surfaces: pan/zoom Lattice org chart, inline Clearance approve/reject, full Costs/Approvals with spend enforcement controls, full issue-as-live-chat-thread detail with run transcripts/interactions, Settings hub, and websocket-driven realtime.

## Remaining External-Infra Work

- Smarter companion: replace the rule parser with an LLM that uses the same governed spine APIs.
- Sandboxed Cell: container/VM isolation before exposing powerful Macro/Rig execution broadly.
- Persistent Keeper and Bench backends behind the in-memory ledgers.
