# Product-spine — implementation map

> **Implementation reference** (complements the idea-layer docs: [`relix-company-model.md`](relix-company-model.md), [`relix-lexicon.md`](relix-lexicon.md), [`relix-agent-adapters.md`](relix-agent-adapters.md), [`relix-hermes-integration.md`](relix-hermes-integration.md)). This maps the lexicon to the *shipped* code: modules + mesh capabilities. All entries are live with passing tests on `codex/product-spine-roadmap`.

## Modules (where the spine lives)

| Module | What |
|---|---|
| `coordinator/agent/store.rs` | Operatives (agent profiles): `reports_to` (Lead), `rig`, `monthly_allowance_cents`; org-tree queries (direct reports / Branch subtree / Line); `manages`; status counts; the hire flow (request/approve/reject). |
| `coordinator/spine/` | Mandates, Campaigns, Guilds (+ Allowance), and the strategy gate — tenant-scoped store + `mandate.*`/`campaign.*`/`guild.*` handlers. |
| `coordinator/brief.rs` | Brief board state machine (`board_transition_allowed`), priorities, the `BriefCard` / `Dossier` / `BriefFields` types. |
| `coordinator/mod.rs` (TaskStore) | The Brief ledger: board-status moves, Claim (lease/heartbeat/release), Sub-briefs/Snags, Dossiers, spine-fields, board/ready/children-done/blocked/stale queries, progress rollups, link listings, chronicle events. |
| `coordinator/heartbeat.rs` | The dispatch loop: `claim_ready_batch`, `dispatch_batch` (claim → run-on-Rig → advance board → release), bridge-token mint/revoke per Shift. |
| `rig/` | The universal agent-backend contract (`Rig` trait), registry, `EchoRig`, `ProcessRig`, the Claude/Codex/Gemini subscription adapters, and the bridge-back token store. |
| `macros/` | The **Macro** (native execute_code): `run_macro` — one script, one capped result. |
| `tradecraft/` | The **Keeper**: usage-clock Knack aging + provenance gate; the creation trigger + post-response nudge. |
| `bench/` | The **Bench**: serverless sleep/wake workspace lifecycle (hibernate to ~$0, wake with snapshot). |
| `controller_runtime.rs` | Wiring: spine handlers, the shared Rig registry + `rig.list`, and the opt-in live heartbeat loop (`RELIX_HEARTBEAT_ENABLED`). |

## Capabilities (live on the mesh, in our language)

**Guild** — `guild.get` · `guild.counts` · `guild.set` · `guild.set_allowance`
**Mandate** — `mandate.create/get/list/update` · `mandate.children` · `mandate.progress` · `mandate.briefs` · `mandate.propose_strategy/approve_strategy/reject_strategy/strategy`
**Campaign** — `campaign.create/get/list/update` · `campaign.progress` · `campaign.briefs`
**Brief** — `brief.move` (board) · `brief.set`/`brief.fields` · `brief.detail` (full view) · `brief.board`/`brief.board_summary` · `brief.desk` (per-Operative) · `brief.workload` · `brief.comment` · `brief.ready` · `brief.children_done` · `brief.blocked`/`brief.blocked_list` · `brief.stale_list` · `brief.subbrief`/`brief.unsubbrief`/`brief.subbriefs` · `brief.parents` (reverse) · `brief.snag`/`brief.unsnag`/`brief.snags` · `brief.blocking` (reverse) · `brief.dossier_add`/`brief.dossiers`/`brief.dossier_get` · `brief.claim`/`brief.heartbeat`/`brief.release`/`brief.claim_holder` · (plus the existing `task.*` execution surface)
**Operative / Roster** — `agent.create/get/list/update/delete/keys` · `agent.reports`/`agent.branch`/`agent.line` (org tree, cycle-guarded) · `agent.manages` · `agent.roster_summary` · `agent.allowance_committed` · the hire flow on the agent status machine
**Rig** — `rig.list` · `rig.describe` (name + label + governance) · per-Operative `rig` field; `dispatch_batch` runs a Brief on its Rig
**Chronicle** — `brief.board_moved` · `brief.assigned` · `brief.comment` · `brief.subbrief_added` · `brief.subbrief_removed` · `brief.snagged` · `brief.snag_cleared` · `brief.dossier_added` · `brief.dispatch_failed`

## Governance & security carried through
- Default-deny agent gate; a **pending** hire is inert (gate denies non-active).
- Tenant-scoped spine reads (a Guild can't read another's Mandates/Campaigns).
- Org tree is **cycle-guarded** — a `reports_to` edge that would close a loop is rejected.
- Mesh tools lent to a Rig route through the admission pipeline; the **box is the boundary** for thin Rigs; the **bridge-back token** is scoped per Shift (Brief + Operative) **and optionally per-method** (`mint_scoped`/`authorize_method`), revoked when the Shift ends.
- The **Macro** (execute_code) runs only **allowlisted interpreters** (`run_macro_guarded`); a `ProcessRig`'s stdout is **capped** so a runaway CLI can't flood context.
- An unrecoverable dispatch **Failed** parks the Brief in `blocked` (with the reason chronicled) instead of re-dispatching forever.
- The enforced **strategy gate** (`strategy_approved`) — a CEO can't build a team until the plan is approved.

## What's primitives-ready but not yet a full feature
- **Phase 5 chat companion** — all the company-aware reads (`*_summary`, `*.briefs`, board/roster) and the materialize surface (create Briefs/Mandates) exist; the AI companion that composes them is the integration.
- **Phase 6 dashboard** — the React SPA renders these capabilities; the frontend is the remaining build.
- **Macro RPC-to-tools** + **learning-loop store integration** + **Hermes Rig adapter** — the cores (`run_macro`, the Keeper decisions, the Rig contract + bridge token) are in; the deeper wiring is next.
