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
| `rig/` | The universal agent-backend contract (`Rig` trait), registry with Guild-default `resolve`, `EchoRig`, `ProcessRig` (stdout-capped, configurable governance), the Claude/Codex/Gemini subscription adapters + the **Hermes** deep adapter (`hermes_rig`, PerToolCall), and the bridge-back token store (per-method scope). |
| `macros/` | The **Macro** (native execute_code): `run_macro` (capped) + `run_macro_guarded` (interpreter allowlist) + `run_macro_rpc` (split `@relix-call` tool requests from residual); `cwd` + scoped `env` for the Cell. |
| `tradecraft/` | The **Keeper**: usage-clock Knack aging + provenance gate; the creation trigger + post-response nudge. |
| `bench/` | The **Bench**: serverless sleep/wake workspace lifecycle (hibernate to ~$0, wake with snapshot); `idle_active_benches` + `hibernate_idle` auto-sleep tick. |
| `src/controller_runtime.rs` (crate root, not under `nodes/coordinator/`) | Wiring: spine handlers, the shared Rig registry + `rig.list`/`rig.describe` (+ `RELIX_DEFAULT_RIG`), and the opt-in live heartbeat loop (`RELIX_HEARTBEAT_ENABLED`) with rich prompt composition, failure-parking, and per-tick token sweep. |
| `relix-cli` `call.rs` | `relix call --method <name> --arg <pipe-delimited>` — generic capability invocation, the operator escape hatch reaching the whole spine surface from the CLI. |
| `relix-web-bridge` `spine.rs` | The dashboard HTTP surface — `GET /v1/spine/{guild,board,board/:col,roster,mandates,mandates/search,mandates/:id/{tree,briefs},briefs/search,briefs/:id,desk/:agent,overdue}` + write `POST /v1/spine/{briefs,briefs/:id/{move,pin,comment,due},mandates}`, all proxying to the coordinator through the mesh admission pipeline. |
| `relix-web-bridge` `spine_dashboard.html` | **Phase 6** — the served `/spine` board page (self-contained inline HTML/JS/CSS, B&W): Board (kanban + detail panel: move/pin/comment/assign/priority/snag/subbrief + create + search + label filter), Mandates (goal tree + create), Roster, and Activity (live chronicle) tabs, plus the companion command bar — all driven by `/v1/spine/*`. |
| `relix-web-bridge` `companion.rs` | **Phase 5** (materialize-work half) — a tested, rule-based command parser (`create brief/mandate`, `move … to …`, `search`, `overdue`, `board`, `help`) behind `POST /v1/spine/companion`. Not an LLM; the verifiable execution spine a model can later sit on. |

## Capabilities (live on the mesh, in our language)

**Guild** — `guild.get` · `guild.counts` · `guild.set` · `guild.set_allowance`
**Mandate** — `mandate.create/get/list/update` · `mandate.children` · `mandate.tree` · `mandate.search` · `mandate.progress` · `mandate.briefs` · `mandate.propose_strategy/approve_strategy/reject_strategy/strategy`
**Campaign** — `campaign.create/get/list/update` · `campaign.search` · `campaign.progress` · `campaign.briefs`
**Brief** — `brief.create` (materialize) · `brief.move` (board) · `brief.set`/`brief.fields` · `brief.detail` (full view) · `brief.search` · `brief.set_labels`/`brief.labels`/`brief.by_label` · `brief.pin` · `brief.set_due`/`brief.due`/`brief.overdue` · `brief.board`/`brief.board_summary` · `brief.desk` (per-Operative) · `brief.workload`/`brief.team_workload` · `brief.subbrief_progress` · `brief.comment` · `brief.ready` · `brief.children_done` · `brief.blocked`/`brief.blocked_list` · `brief.stale_list` · `brief.subbrief`/`brief.unsubbrief`/`brief.subbriefs` · `brief.parents` (reverse) · `brief.snag`/`brief.unsnag`/`brief.snags` · `brief.blocking` (reverse) · `brief.dossier_add`/`brief.dossiers`/`brief.dossier_get`/`brief.dossier_latest` · `brief.claim`/`brief.heartbeat`/`brief.release`/`brief.claim_holder` · (plus the existing `task.*` execution surface)
**Operative / Roster** — `agent.create/get/list/update/delete/keys` · `agent.reports`/`agent.branch`/`agent.line`/`agent.peers` (org tree, cycle-guarded) · `agent.by_role` (staffing) · `agent.manages` · `agent.roster_summary` · `agent.allowance_committed` · the hire flow on the agent status machine
**Rig** — `rig.list` · `rig.describe` (name + label + governance) · per-Operative `rig` field; `dispatch_batch` runs a Brief on its Rig
**Chronicle** — `brief.created` · `brief.board_moved` · `brief.assigned` · `brief.comment` · `brief.subbrief_added` · `brief.subbrief_removed` · `brief.snagged` · `brief.snag_cleared` · `brief.dossier_added` · Shift lifecycle: `brief.shift_done` / `brief.continued` / `brief.dispatch_failed`

## Governance & security carried through
- Default-deny agent gate; a **pending** hire is inert (gate denies non-active).
- Tenant-scoped spine reads (a Guild can't read another's Mandates/Campaigns).
- Org tree is **cycle-guarded** — a `reports_to` edge that would close a loop is rejected.
- Mesh tools lent to a Rig route through the admission pipeline; the **box is the boundary** for thin Rigs; the **bridge-back token** is scoped per Shift (Brief + Operative) **and optionally per-method** (`mint_scoped`/`authorize_method`), revoked when the Shift ends.
- The **Macro** (execute_code) runs only **allowlisted interpreters** (`run_macro_guarded`); a `ProcessRig`'s stdout is **capped** so a runaway CLI can't flood context.
- An unrecoverable dispatch **Failed** parks the Brief in `blocked` (with the reason chronicled) instead of re-dispatching forever.
- The **strategy gate** is a *tenant-guarded, queryable* predicate (`strategy_approved`) with the proposed→approved/rejected state machine. ⚠️ It is **not yet enforced** — no hire/team-build path blocks on it (company-model §10.3 wants enforcement; that coupling is follow-up). Do not describe it as enforced until wired.

## Shipped this roadmap
- **Phase 6 dashboard** — served at `/spine`, fully functional (Board/Mandates/Roster/Activity + command bar) over the `/v1/spine/*` API.
- **Phase 5 materialize-work** — the rule-based companion (`/v1/spine/companion`) + command bar create/move/search/etc. through the spine.
- **Macro RPC-to-tools** parse layer (`extract_tool_calls`/`run_macro_rpc`); **Hermes Rig adapter** (`hermes_rig`, PerToolCall); the **Keeper** (`KnackLedger`) runnable in-memory.

## Remaining (needs external infra, not a tested-Rust slice)
- **Smarter companion** — swap the rule-based parser for an LLM that composes the same `/v1/spine/*` execution path (needs a model configured).
- **Sandboxed Cell** — container/VM isolation to safely expose `macro.run` on the mesh (the Macro core + interpreter allowlist + cwd/scoped-env are in).
- **Persistent Keeper / Bench backends** — SQLite/worktree stores layered over the in-memory `KnackLedger` / `BenchLedger`.
