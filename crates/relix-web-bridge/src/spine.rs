//! Product-spine HTTP surface for the dashboard.
//!
//! Thin read proxies that dial the coordinator and forward the
//! product-spine capabilities (`brief.*` / `mandate.*` / `agent.*`
//! summaries) to the browser as JSON. Every call goes through the
//! mesh admission pipeline via the bridge identity, exactly like the
//! `agent.*` / `task.*` routes — these add no new trust, just a
//! browser-friendly shape over the existing capabilities.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

/// The coordinator's mesh alias (same as the `agent.*` routes).
const DEFAULT_PEER: &str = "coordinator";

/// `GET /spine` — RETIRED (Phase 2 Slice 2). The interim spine board
/// (`spine_dashboard.html`) has been deleted: the React dashboard in
/// `apps/dashboard` (served at `/dashboard`) is the one canonical product
/// surface, and every useful capability the old board had now lives in React
/// (Briefs board + Brief detail/Chronicle, Crew + Operative Keys/Allowance,
/// Mandates + Clearances, Command Center inbox, Runs). `/spine` is now a
/// permanent redirect to `/dashboard` — kept (not removed) so old bookmarks
/// and docs keep working. The `/v1/spine/*` JSON routes below are the real,
/// supported product-spine API and are unchanged.
pub async fn page() -> Response {
    // 308 Permanent: the board is gone for good, so the redirect is
    // permanent. Method is preserved (a GET stays a GET).
    Redirect::permanent("/dashboard").into_response()
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Optional status filter for `mandate.list`.
    #[serde(default)]
    pub status: Option<String>,
    /// Optional limit (search / list / overdue).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional free-text query for the search routes.
    #[serde(default)]
    pub q: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AssignCheckQuery {
    /// The Operative that would do the assigning.
    #[serde(default)]
    pub actor: Option<String>,
    /// The proposed assignee.
    #[serde(default)]
    pub assignee: Option<String>,
}

// ── routes ────────────────────────────────────────────────

/// `GET /v1/spine/guild` — the Guild's Mandate/Campaign rollup.
pub async fn guild_counts(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "guild.counts", b"").await?)
}

/// `GET /v1/spine/board` — Brief counts by board column.
/// `GET /v1/spine/guild/detail` - Guild profile, including monthly
/// Allowance when configured.
pub async fn guild_detail(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "guild.get", b"").await?)
}

/// `GET /v1/spine/allowance/committed` - total monthly Allowance
/// committed to Operatives.
pub async fn allowance_committed(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "agent.allowance_committed", b"").await?)
}

pub async fn board_summary(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "brief.board_summary", b"").await?)
}

/// `GET /v1/spine/roster` — Operative counts by status.
pub async fn roster_summary(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "agent.roster_summary", b"").await?)
}

/// `GET /v1/spine/mandates?status=` — Mandates (optionally filtered).
pub async fn mandates(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = q.status.unwrap_or_default();
    json_passthrough(call_peer(&state, "mandate.list", arg.as_bytes()).await?)
}

/// `GET /v1/spine/mandates/search?q=&limit=` — Mandate title search.
pub async fn mandate_search(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let query = q.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Err(bad("q (query) required"));
    }
    let arg = format!("{}|{}", query, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "mandate.search", arg.as_bytes()).await?)
}

/// `GET /v1/spine/mandates/:id/tree` — a Mandate with its direct
/// sub-Mandates and Campaigns.
pub async fn mandate_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "mandate.tree", id.as_bytes()).await?)
}

/// `GET /v1/spine/mandates/:id/briefs` — the Briefs under a Mandate.
pub async fn mandate_briefs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{}|{}", id, q.limit.unwrap_or(100));
    json_passthrough(call_peer(&state, "mandate.briefs", arg.as_bytes()).await?)
}

/// `GET /v1/spine/briefs/:id` — the full Brief detail view.
pub async fn brief_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "brief.detail", id.as_bytes()).await?)
}

/// `GET /v1/spine/board/:column?limit=` — the Briefs in one column.
/// `GET /v1/spine/briefs/:id/wakeups?limit=` - the Brief wakeup ledger.
pub async fn brief_wakeups(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{}|{}", id, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.wakeups", arg.as_bytes()).await?)
}

pub async fn board_column(
    State(state): State<AppState>,
    Path(column): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{}|{}", column, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.board", arg.as_bytes()).await?)
}

/// `GET /v1/spine/desk/:agent?limit=` — an Operative's in-flight Briefs.
pub async fn desk(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{}|{}", agent, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.desk", arg.as_bytes()).await?)
}

/// `GET /v1/spine/by-label?q=&limit=` — Briefs carrying a label.
pub async fn by_label(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let label = q.q.unwrap_or_default();
    if label.trim().is_empty() {
        return Err(bad("q (label) required"));
    }
    let arg = format!("{}|{}", label, q.limit.unwrap_or(100));
    json_passthrough(call_peer(&state, "brief.by_label", arg.as_bytes()).await?)
}

/// `GET /v1/spine/overdue?limit=` — the overdue Briefs.
pub async fn overdue(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("|{}", q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.overdue", arg.as_bytes()).await?)
}

/// `GET /v1/spine/blocked?limit=` — Briefs blocked by an unresolved Snag.
pub async fn blocked(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = q.limit.unwrap_or(50).to_string();
    json_passthrough(call_peer(&state, "brief.blocked_list", arg.as_bytes()).await?)
}

/// `GET /v1/spine/stale?idle_secs=&limit=` — Briefs idle in an active column.
pub async fn stale(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    // brief.stale_list arg is `idle_secs|limit`; default 1 day.
    let arg = format!("86400|{}", q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.stale_list", arg.as_bytes()).await?)
}

/// `GET /v1/spine/unblocked?limit=` — the blockers-resolved wake list.
pub async fn unblocked(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = q.limit.unwrap_or(50).to_string();
    json_passthrough(call_peer(&state, "brief.unblocked", arg.as_bytes()).await?)
}

/// `GET /v1/spine/briefs/search?q=&limit=` — Brief title search.
pub async fn brief_search(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let query = q.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Err(bad("q (query) required"));
    }
    let arg = format!("{}|{}", query, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.search", arg.as_bytes()).await?)
}

/// `GET /v1/spine/unassigned?limit=` — Briefs in an active column
/// with no assignee (the Desk's "needs staffing" list).
pub async fn unassigned(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = q.limit.unwrap_or(50).to_string();
    json_passthrough(call_peer(&state, "brief.unassigned", arg.as_bytes()).await?)
}

/// `GET /v1/spine/keys/:agent` — the full Operative profile as JSON
/// (identity + the Keys permission surface, including the org/work
/// Keys: can_spawn_agents, spawn_route, can_assign_work, assign_scope,
/// assign_allowed_agents, can_manage_work, can_configure_agents,
/// configure_scope, secret_allowlist, instruction_bundle). Backs the
/// per-Operative Keys panel; edits go through `PATCH /v1/agents/:id`.
pub async fn keys(
    State(state): State<AppState>,
    Path(agent): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "agent.keys", agent.as_bytes()).await?)
}

/// `GET /v1/spine/assign_check?actor=&assignee=` — would `actor` be
/// permitted to assign a Brief to `assignee` under its Keys? Returns
/// the JSON KeyVerdict (`{"decision":"allow"}` /
/// `{"decision":"deny","reason":…}`). Read-only preview of the gate
/// that `brief.set` enforces.
pub async fn assign_check(
    State(state): State<AppState>,
    Query(q): Query<AssignCheckQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let actor = q.actor.unwrap_or_default();
    let assignee = q.assignee.unwrap_or_default();
    if actor.trim().is_empty() || assignee.trim().is_empty() {
        return Err(bad("actor and assignee required"));
    }
    let arg = format!("{}|{}", actor.trim(), assignee.trim());
    json_passthrough(call_peer(&state, "agent.assign_check", arg.as_bytes()).await?)
}

/// `GET /v1/spine/clearances?limit=` — the pending Clearances
/// (approvals awaiting a Founder greenlight) as a JSON array. Sourced
/// from `coord.approval.pending`, whose TSV lines are parsed into
/// objects so the Desk can render them. Read-only: approve/reject is
/// not proxied here (`coord.approval.decide` needs an authorised
/// approver identity the bridge does not yet forward — documented).
pub async fn clearances(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let limit = q.limit.unwrap_or(25).min(100);
    let raw = call_peer(
        &state,
        "coord.approval.pending",
        limit.to_string().as_bytes(),
    )
    .await?;
    json_value(parse_clearance_lines(&raw))
}

#[derive(Debug, Deserialize, Default)]
pub struct ClearanceDecision {
    /// `approve` / `reject` (also accepts `approved` / `rejected`).
    #[serde(default)]
    pub decision: String,
    /// Optional operator note recorded on the decision.
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /v1/spine/clearances/:approval_id/decide` — greenlight or
/// refuse a pending Clearance inline from the Desk. Body:
/// `{ "decision": "approve"|"reject", "note"?: "..." }`.
///
/// The bridge forwards to `coord.approval.decide` **as its own
/// verified identity** — exactly like the existing
/// `/v1/approvals/:id/decide` route. The runtime cap enforces the
/// real authorisation (`authorized_approvers` ∪ operator/admin role),
/// so this never fabricates approval: an unauthorised bridge identity
/// is refused by the cap, not waved through here. Tenant scoping means
/// the approval must belong to the caller's Guild, and the runtime
/// refuses to re-decide an already-terminal approval (so side effects —
/// e.g. activating a spawn hire — apply exactly once). The minted
/// token's TTL is controller-configured; there is intentionally no
/// per-decision TTL knob.
pub async fn decide_clearance(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Json(req): Json<ClearanceDecision>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if approval_id.trim().is_empty() {
        return Err(bad("approval_id required"));
    }
    let decision = match normalize_clearance_decision(&req.decision) {
        Some(d) => d,
        None => {
            return Err(bad(&format!(
                "decision must be approve|reject, got `{}`",
                req.decision
            )));
        }
    };
    // coord.approval.decide arg: approval_id|decision|decided_by|note?
    let note = req.note.unwrap_or_default();
    let arg = format!("{}|{decision}|operator|{note}", approval_id.trim());
    let raw = call_peer(&state, "coord.approval.decide", arg.as_bytes()).await?;
    // Response is `ok\n` or `ok|<token>\n`.
    let text = String::from_utf8_lossy(&raw);
    let token = text
        .trim()
        .strip_prefix("ok|")
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    json_value(serde_json::json!({
        "ok": true,
        "approval_id": approval_id.trim(),
        "decision": decision,
        "approval_token": token,
    }))
}

// ── Prime Assistant ("describe what you want → plan") ────────────────

#[derive(Debug, Deserialize)]
pub struct PrimeProposeRequest {
    pub message: String,
}

/// `POST /v1/spine/prime/propose` — interpret a request into a structured,
/// READ-ONLY plan (creates nothing but the proposal record). Tenant-scoped.
pub async fn prime_propose(
    State(state): State<AppState>,
    Json(req): Json<PrimeProposeRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let msg = req.message.trim();
    if msg.is_empty() {
        return Err(bad("message required"));
    }
    json_passthrough(call_peer(&state, "prime.propose", msg.as_bytes()).await?)
}

#[derive(Debug, Deserialize)]
pub struct PrimeApproveRequest {
    pub proposal_id: String,
}

/// `POST /v1/spine/prime/approve` — the ONLY path that materializes a Prime
/// proposal (Mandate + Briefs + assignments + pending hire requests). Never
/// runs an adapter, applies a workspace, or changes budget. Tenant-scoped.
pub async fn prime_approve(
    State(state): State<AppState>,
    Json(req): Json<PrimeApproveRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let id = req.proposal_id.trim();
    if id.is_empty() {
        return Err(bad("proposal_id required"));
    }
    json_passthrough(call_peer(&state, "prime.approve", id.as_bytes()).await?)
}

#[derive(Debug, Deserialize)]
pub struct PrimeStartRequest {
    pub proposal_id: String,
    /// Optional cap on how many ready Briefs to start in this call (the rest
    /// are reported skipped, never silently dropped). Defaults server-side.
    #[serde(default)]
    pub max: Option<usize>,
}

/// `POST /v1/spine/prime/start` — Start-to-Shift (company-model §12.5B). Turns
/// an APPROVED proposal's READY Briefs into real Shifts through the same run
/// chokepoint as `brief.run`. Creates no Mandate/Brief/hire and changes no
/// budget — it only RUNS Briefs that are already assigned, active, and
/// unblocked; every skipped Brief is returned with an honest reason. Returns
/// `{proposal_id, mandate_id, started:[{brief_id,run_id,rig,status}], skipped}`.
/// Tenant-scoped: a non-approved / unknown / cross-Guild proposal is refused.
pub async fn prime_start(
    State(state): State<AppState>,
    Json(req): Json<PrimeStartRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let id = req.proposal_id.trim();
    if id.is_empty() {
        return Err(bad("proposal_id required"));
    }
    // The coordinator capability parses `proposal_id` or `proposal_id|max`.
    // Reject a `|` in the id so the delimiter can't be smuggled.
    if id.contains('|') {
        return Err(bad("invalid proposal id"));
    }
    let arg = match req.max {
        Some(m) if m >= 1 => format!("{id}|{m}"),
        _ => id.to_string(),
    };
    json_passthrough(call_peer(&state, "prime.start", arg.as_bytes()).await?)
}

/// `GET /v1/spine/prime/proposals?limit=N` — recent Prime proposals for the
/// Guild (the companion history).
pub async fn prime_proposals(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = q.limit.unwrap_or(20).to_string();
    json_passthrough(call_peer(&state, "prime.proposals", arg.as_bytes()).await?)
}

/// `GET /v1/spine/prime/proposals/:id` — one proposal (tenant-scoped).
pub async fn prime_proposal(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "prime.proposal", id.as_bytes()).await?)
}

#[derive(Debug, Deserialize, Default)]
pub struct TeamPlanRequest {
    /// Optional plain-text goal/team description.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional CSV of `role` or `role:subject_id` entries.
    #[serde(default)]
    pub roles: Option<String>,
}

/// `POST /v1/spine/mandates/:id/team_plan` — the Prime team-build
/// foundation. Body `{ "description"?, "roles"? }` (roles is a CSV of
/// `role` or `role:subject_id`). Proxies `mandate.team_plan`, which
/// requires an approved strategy + the actor's spawn Key and returns
/// the structured JSON plan. Governed, not autonomous.
pub async fn team_plan(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
    Json(req): Json<TeamPlanRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    let description = req.description.unwrap_or_default();
    let roles = req.roles.unwrap_or_default();
    let arg = format!("{}|{description}|{roles}", mandate_id.trim());
    json_passthrough(call_peer(&state, "mandate.team_plan", arg.as_bytes()).await?)
}

/// `GET /v1/spine/mandates/:id/team_plan` — the latest persisted Team
/// Plan for a Mandate as JSON (`null` if never planned). Proxies
/// `mandate.team_plan.latest`. Tenant-scoped.
pub async fn team_plan_latest(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    json_passthrough(
        call_peer(
            &state,
            "mandate.team_plan.latest",
            mandate_id.trim().as_bytes(),
        )
        .await?,
    )
}

/// `GET /v1/spine/mandates/:id/team_readiness` — live team readiness
/// for a Mandate (plan + current hire/Clearance states). Proxies
/// `mandate.team_readiness`. Tenant-scoped.
pub async fn team_readiness(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    json_passthrough(
        call_peer(
            &state,
            "mandate.team_readiness",
            mandate_id.trim().as_bytes(),
        )
        .await?,
    )
}

#[derive(Debug, Deserialize, Default)]
pub struct OrchestrateRequest {
    /// `plan_only` (default) / `create_briefs` / `assign_ready`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Cap on child Briefs (default 16).
    #[serde(default)]
    pub max_briefs: Option<usize>,
    /// Report-only when true (default false).
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// `POST /v1/spine/mandates/:id/orchestrate` — Prime Mandate-to-Brief
/// orchestration. Body `{ "mode"?, "max_briefs"?, "dry_run"? }`.
/// Proxies `mandate.orchestrate` (strategy + ready-team gated; creates
/// an idempotent Brief tree and assigns active agents). Tenant-scoped.
pub async fn orchestrate(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
    Json(req): Json<OrchestrateRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    let mode = req.mode.unwrap_or_default();
    let max_briefs = req.max_briefs.map(|n| n.to_string()).unwrap_or_default();
    let dry_run = req.dry_run.unwrap_or(false);
    let arg = format!("{}|{mode}|{max_briefs}|{dry_run}", mandate_id.trim());
    json_passthrough(call_peer(&state, "mandate.orchestrate", arg.as_bytes()).await?)
}

/// `GET /v1/spine/mandates/:id/orchestration/latest` — the latest
/// persisted orchestration run for a Mandate (`null` if never run).
/// Proxies `mandate.orchestration.latest`. Tenant-scoped.
pub async fn orchestration_latest(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    json_passthrough(
        call_peer(
            &state,
            "mandate.orchestration.latest",
            mandate_id.trim().as_bytes(),
        )
        .await?,
    )
}

#[derive(Debug, Deserialize, Default)]
pub struct ProposeStrategyRequest {
    #[serde(default)]
    pub doc: Option<String>,
}

/// `GET /v1/spine/mandates/:id/strategy` — the Mandate strategy status
/// (`proposed`/`approved`/`rejected`/null). Proxies `mandate.strategy.status`.
pub async fn strategy_status(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    json_passthrough(call_peer(&state, "mandate.strategy.status", mandate_id.trim().as_bytes()).await?)
}

/// `POST /v1/spine/mandates/:id/strategy/propose` — set/replace the strategy
/// to `proposed`. Body `{ "doc"? }` (a default is used if omitted). Proxies
/// `mandate.strategy.propose`. Does NOT bypass governance — approval is a
/// separate explicit step.
pub async fn strategy_propose(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
    Json(req): Json<ProposeStrategyRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    let doc = req
        .doc
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "Plan and execute this Mandate.".to_string());
    if doc.contains('|') {
        return Err(bad("strategy doc must not contain `|`"));
    }
    let arg = format!("{}|{doc}", mandate_id.trim());
    json_passthrough(call_peer(&state, "mandate.strategy.propose", arg.as_bytes()).await?)
}

/// `POST /v1/spine/mandates/:id/strategy/approve` — approve a proposed
/// strategy. Proxies `mandate.strategy.approve`.
pub async fn strategy_approve(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    json_passthrough(call_peer(&state, "mandate.strategy.approve", mandate_id.trim().as_bytes()).await?)
}

/// `POST /v1/spine/mandates/:id/strategy/reject` — reject a proposed
/// strategy. Proxies `mandate.strategy.reject`.
pub async fn strategy_reject(
    State(state): State<AppState>,
    Path(mandate_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if mandate_id.trim().is_empty() {
        return Err(bad("mandate_id required"));
    }
    json_passthrough(call_peer(&state, "mandate.strategy.reject", mandate_id.trim().as_bytes()).await?)
}

/// Normalise a Clearance decision into the wire value
/// `coord.approval.decide` expects. Accepts the product verbs
/// (`approve`/`reject`) and the raw runtime values
/// (`approved`/`rejected`); anything else is `None` → a 400.
fn normalize_clearance_decision(decision: &str) -> Option<&'static str> {
    match decision.trim() {
        "approve" | "approved" => Some("approved"),
        "reject" | "rejected" => Some("rejected"),
        _ => None,
    }
}

/// `GET /v1/spine/briefs/:id/events?limit=` — a single Brief's
/// Chronicle (newest first), as a JSON array. Bounded by `limit`.
pub async fn brief_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let limit = q.limit.unwrap_or(200).min(500);
    // task.events arg: task_id|after_id|limit|type|order
    let arg = format!("{id}|0|{limit}||desc");
    let raw = call_peer(&state, "task.events", arg.as_bytes()).await?;
    json_value(parse_event_lines(&raw))
}

/// `GET /v1/spine/inbox?limit=` — a single Desk/Inbox payload: the
/// real "needs attention" surfaces (blocked, stale, overdue, in
/// review, unassigned) in one bounded response. Each section is a
/// JSON array of Brief cards; no fabricated data, no counters.
pub async fn inbox(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let limit = q.limit.unwrap_or(25).min(100);
    let blocked = call_peer(&state, "brief.blocked_list", limit.to_string().as_bytes()).await?;
    let stale = call_peer(
        &state,
        "brief.stale_list",
        format!("86400|{limit}").as_bytes(),
    )
    .await?;
    let overdue = call_peer(&state, "brief.overdue", format!("|{limit}").as_bytes()).await?;
    let review = call_peer(
        &state,
        "brief.board",
        format!("in_review|{limit}").as_bytes(),
    )
    .await?;
    let unassigned = call_peer(&state, "brief.unassigned", limit.to_string().as_bytes()).await?;
    let body = serde_json::json!({
        "blocked": parse_json(&blocked),
        "stale": parse_json(&stale),
        "overdue": parse_json(&overdue),
        "review": parse_json(&review),
        "unassigned": parse_json(&unassigned),
    });
    json_value(body)
}

/// `GET /v1/spine/briefs/:id/thread?limit=` — the Brief live work
/// thread in one payload: the full detail (fields, snags, sub-briefs,
/// parents, dossiers, labels, due, pinned, blocked), the Chronicle
/// timeline (newest first, bounded), the wakeup ledger, and the
/// current Claim holder. Composes existing capabilities; no new
/// runtime logic, no fabricated data.
pub async fn brief_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let limit = q.limit.unwrap_or(100).min(500);
    // `brief.detail` errors (not found) propagate, so a bad id 404s.
    let detail = call_peer(&state, "brief.detail", id.as_bytes()).await?;
    let events = call_peer(
        &state,
        "task.events",
        format!("{id}|0|{limit}||desc").as_bytes(),
    )
    .await
    .unwrap_or_default();
    let wakeups = call_peer(&state, "brief.wakeups", format!("{id}|50").as_bytes())
        .await
        .unwrap_or_default();
    let claim = call_peer(&state, "brief.claim_holder", id.as_bytes())
        .await
        .unwrap_or_default();
    let body = serde_json::json!({
        "detail": parse_json(&detail),
        "events": parse_event_lines(&events),
        "wakeups": parse_json(&wakeups),
        "claim": parse_json(&claim),
    });
    json_value(body)
}

// ── composite helpers ─────────────────────────────────────

/// Parse a JSON mesh body into a `Value`; empty / unparseable → Null.
fn parse_json(body: &[u8]) -> serde_json::Value {
    if body.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
}

/// Parse `coord.approval.pending`'s TSV rows
/// (`approval_id\tagent_id\tmethod\treason\trequested_at`) into a JSON
/// array of objects. The trailing `count=N` line is dropped.
fn parse_clearance_lines(body: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(body);
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("count="))
        .map(|l| {
            let mut cols = l.split('\t');
            serde_json::json!({
                "approval_id": cols.next().unwrap_or("").to_string(),
                "agent_id": cols.next().unwrap_or("").to_string(),
                "method": cols.next().unwrap_or("").to_string(),
                "reason": cols.next().unwrap_or("").to_string(),
                "requested_at": cols.next().unwrap_or("").to_string(),
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}

/// Parse `task.events`' newline-delimited JSON objects into an array.
fn parse_event_lines(body: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(body);
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    serde_json::Value::Array(rows)
}

/// Serialize a composed `Value` into a JSON `200` response.
fn json_value(v: serde_json::Value) -> Result<Response, (StatusCode, Json<ApiError>)> {
    match serde_json::to_vec(&v) {
        Ok(b) => json_passthrough(b),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("encode: {e}"),
            }),
        )),
    }
}

// ── write routes ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateBriefRequest {
    pub title: String,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub mandate: Option<String>,
    #[serde(default)]
    pub campaign: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
}

/// `POST /v1/spine/briefs` — materialize a Brief. Returns the id.
pub async fn create_brief(
    State(state): State<AppState>,
    Json(req): Json<CreateBriefRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if req.title.trim().is_empty() {
        return Err(bad("title required"));
    }
    let opt = |o: &Option<String>| o.clone().unwrap_or_default();
    // The wire arg is pipe-delimited; none of these positional fields
    // may contain a literal `|` or they'd shift the arg layout.
    for (label, val) in [
        ("title", req.title.as_str()),
        ("assignee", &opt(&req.assignee)),
        ("mandate", &opt(&req.mandate)),
        ("campaign", &opt(&req.campaign)),
        ("priority", &opt(&req.priority)),
    ] {
        if val.contains('|') {
            return Err(bad(&format!("{label} must not contain `|`")));
        }
    }
    let arg = format!(
        "{}|{}|{}|{}|{}",
        req.title,
        opt(&req.assignee),
        opt(&req.mandate),
        opt(&req.campaign),
        opt(&req.priority)
    );
    let body = call_peer(&state, "brief.create", arg.as_bytes()).await?;
    json_id("task_id", &body)
}

#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub status: String,
}

/// `POST /v1/spine/briefs/:id/move` — move a Brief on the board.
pub async fn move_brief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<MoveRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{id}|{}", req.status);
    call_peer(&state, "brief.move", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize, Default)]
pub struct RunBriefRequest {
    /// Optional Rig override — forces a specific adapter (e.g. `echo` for the
    /// golden-path smoke) instead of the assignee's configured Rig. Empty /
    /// absent uses the Operative's Rig.
    #[serde(default)]
    pub rig: Option<String>,
}

/// `POST /v1/spine/briefs/:id/run` — run a Brief NOW through its
/// Operative's agent adapter (Rig), or an explicit `{rig}` override.
/// Returns the structured RunReport (`status` = done/failed/continued or
/// a clear unavailable refusal, plus `rig`, `summary`, optional
/// `install_hint`). The execution result is also chronicled on the Brief
/// (read back via `/v1/spine/briefs/:id/events`).
pub async fn run_brief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<RunBriefRequest>>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let rig = body
        .and_then(|Json(b)| b.rig)
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());
    // The coordinator capability parses `brief_id` or `brief_id|rig`. Reject
    // a `|` in the brief id so the override delimiter can't be smuggled.
    if id.contains('|') {
        return Err(bad("invalid brief id"));
    }
    let arg = match rig {
        Some(r) if !r.contains('|') => format!("{id}|{r}"),
        Some(_) => return Err(bad("invalid rig override")),
        None => id,
    };
    let resp = call_peer(&state, "brief.run", arg.as_bytes()).await?;
    json_passthrough(resp)
}

/// `GET /v1/runs` — the recent execution runs across all Briefs (the
/// Active Runs feed). Stable structured run records straight from the
/// `brief_runs` ledger — the dashboard polls this to watch a run move
/// `running` → `done`/`failed`/`continued` without parsing event text.
pub async fn runs_recent(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "brief.runs", b"").await?)
}

/// `GET /v1/spine/briefs/:id/runs` — the run (Shift) history for one
/// Brief, newest first.
pub async fn brief_runs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "brief.runs", id.as_bytes()).await?)
}

/// `GET /v1/runs/:run_id` — one run record (detail).
pub async fn run_get(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "run.get", run_id.as_bytes()).await?)
}

/// `GET /v1/runs/:run_id/events` — a run's transcript, chronological.
pub async fn run_events(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "run.events", run_id.as_bytes()).await?)
}

/// `POST /v1/runs/:run_id/cancel` — request cancellation of an in-flight
/// run (kills the live process if still active; truthful about whether it
/// was active).
pub async fn run_cancel(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "run.cancel", run_id.as_bytes()).await?)
}

/// `GET /v1/runs/:run_id/artifacts` — the changed files a run produced.
pub async fn run_artifacts(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "run.artifacts", run_id.as_bytes()).await?)
}

/// `GET /v1/runs/:run_id/artifacts/:artifact_id/preview` — a safe,
/// size-limited text preview of one artifact (refuses binary/large).
pub async fn run_artifact_preview(
    State(state): State<AppState>,
    Path((run_id, artifact_id)): Path<(String, String)>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{run_id}|{artifact_id}");
    json_passthrough(call_peer(&state, "run.artifact_preview", arg.as_bytes()).await?)
}

/// `GET /v1/runs/:run_id/artifacts/:artifact_id/diff` — a safe, bounded
/// unified diff of one changed file (workspace output vs the run's baseline).
/// Returns `available:false` + a reason when no honest diff is possible
/// (binary / moved baseline / unsafe path) so the caller previews instead.
pub async fn run_artifact_diff(
    State(state): State<AppState>,
    Path((run_id, artifact_id)): Path<(String, String)>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{run_id}|{artifact_id}");
    json_passthrough(call_peer(&state, "run.artifact_diff", arg.as_bytes()).await?)
}

#[derive(Debug, Deserialize)]
pub struct ReviewRequest {
    pub decision: String,
    #[serde(default)]
    pub note: String,
}

/// `POST /v1/runs/:run_id/review` — record an operator accept/reject of a
/// run's result. Does NOT apply files back to the project (future, guarded).
pub async fn run_review(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(req): Json<ReviewRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let decision = req.decision.trim();
    if decision != "accepted" && decision != "rejected" {
        return Err(bad("decision must be `accepted` or `rejected`"));
    }
    if req.note.contains('|') {
        return Err(bad("note must not contain `|`"));
    }
    let arg = format!("{run_id}|{decision}|{}", req.note);
    json_passthrough(call_peer(&state, "run.review", arg.as_bytes()).await?)
}

/// `GET /v1/runs/:run_id/diff` — the safe-apply PLAN for a run (per-file
/// action / conflict + eligibility). Pure preview: never mutates files.
pub async fn run_diff(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "run.diff", run_id.as_bytes()).await?)
}

/// `POST /v1/runs/:run_id/apply` — apply an accepted run's changed files
/// back into the configured project root. Refuses the whole apply if any
/// file is unsafe / conflicted (no partial apply, no `force`).
pub async fn run_apply(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "run.apply", run_id.as_bytes()).await?)
}

/// `POST /v1/runs/:run_id/discard` — discard a terminal run's output: marks it
/// `discarded` (rejecting a `done` run's review so it can never be applied),
/// records a Chronicle + transcript event, and makes its scoped workspace
/// eligible for the normal storage prune. Does NOT delete files immediately.
pub async fn run_discard(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "run.discard", run_id.as_bytes()).await?)
}

#[derive(Debug, Deserialize)]
pub struct RuntimeStateQuery {
    #[serde(default)]
    pub agent_id: String,
}

/// `GET /v1/runs/runtime-state?agent_id=...` — the persisted adapter runtime
/// state rows for one agent (resumable session id, accumulated usage/cost,
/// last run status). Tenant-scoped; newest first.
pub async fn runtime_state_get(
    State(state): State<AppState>,
    Query(q): Query<RuntimeStateQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let agent_id = q.agent_id.trim();
    if agent_id.is_empty() {
        return Err(bad("agent_id query parameter is required"));
    }
    json_passthrough(call_peer(&state, "rig.runtime_state.get", agent_id.as_bytes()).await?)
}

#[derive(Debug, Deserialize)]
pub struct RuntimeStateResetRequest {
    pub agent_id: String,
    #[serde(default)]
    pub brief_key: Option<String>,
}

/// `POST /v1/runs/runtime-state/reset` — forget persisted adapter runtime
/// state for one agent (optionally scoped to a single Brief via `brief_key`).
/// Tenant-scoped. Returns `{"removed": <count>}`.
pub async fn runtime_state_reset(
    State(state): State<AppState>,
    Json(req): Json<RuntimeStateResetRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if req.agent_id.trim().is_empty() {
        return Err(bad("agent_id is required"));
    }
    let mut body = serde_json::json!({ "agent_id": req.agent_id.trim() });
    if let Some(bk) = req.brief_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        body["brief_key"] = serde_json::Value::String(bk.to_string());
    }
    let arg = serde_json::to_vec(&body).map_err(|e| bad(&format!("encode: {e}")))?;
    json_passthrough(call_peer(&state, "rig.runtime_state.reset", &arg).await?)
}

/// `GET /v1/spine/company` — first-run status: whether the Guild has a
/// Founder yet, the Founder profile, and the Operative count. The
/// dashboard reads this to show the "Initialize Company" first-run state.
pub async fn company_status(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "company.status", b"").await?)
}

/// `GET /v1/spine/operatives` — the Crew roster (real Operatives in the
/// Guild, with their adapter/rig). Excludes the infra operator-console.
pub async fn operatives(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "agent.operatives", b"").await?)
}

/// `GET /v1/spine/run-config` — the run-workspace context config (mode /
/// project root / caps) the dashboard Settings shows so an operator sees
/// how Brief runs are sandboxed.
pub async fn run_config(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "run.workspace_config", b"").await?)
}

/// `GET /v1/maintenance/summary` — operator storage + run-ledger overview
/// (workspace count/bytes, run/event/artifact counts, warnings). Bounded,
/// no secrets. Auth-gated by the bridge middleware like every `/v1/*`.
pub async fn maintenance_summary(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "maintenance.summary", b"").await?)
}

/// `POST /v1/maintenance/prune` — safe prune of OLD run workspaces (and,
/// optionally, the verbose log rows of pruned runs). Dry-run by default;
/// a real delete needs `{"dry_run": false}`. The raw JSON body is forwarded
/// to the runtime, which parses the options + enforces every safety rule.
pub async fn maintenance_prune(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let bytes = if body.is_empty() { b"{}".to_vec() } else { body.to_vec() };
    json_passthrough(call_peer(&state, "maintenance.prune", &bytes).await?)
}

/// `GET /v1/maintenance/audit?limit=N` — recent maintenance-audit rows
/// (newest first): when cleanup ran, what it deleted, the trigger, and the
/// status. Auth-gated by the bridge middleware.
pub async fn maintenance_audit(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    json_passthrough(call_peer(&state, "maintenance.audit", limit.to_string().as_bytes()).await?)
}

#[derive(Debug, Deserialize, Default)]
pub struct CompanyInitRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub rig: Option<String>,
}

/// `POST /v1/spine/company/init` — first-run owner action: create the
/// Guild's single Founder Operative (idempotent — a repeat returns the
/// existing Founder, never a duplicate). **Owner-gated at the bridge**:
/// requires a logged-in dashboard session (the admin set up on first
/// run), NOT merely a bearer token, so only the dashboard owner can
/// initialise the company. The runtime capability *also* gates on the
/// console/operator identity, so this is defence-in-depth.
pub async fn company_init(
    State(state): State<AppState>,
    headers: header::HeaderMap,
    Json(req): Json<CompanyInitRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let session_ok = crate::dashboard_auth::session_cookie_from_headers(&headers)
        .and_then(|sid| state.dashboard_auth.validate_session(&sid))
        .is_some();
    if !session_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "initializing the company requires a logged-in dashboard session".into(),
            }),
        ));
    }
    let name = req.name.unwrap_or_default();
    let rig = req.rig.unwrap_or_default();
    if name.contains('|') || rig.contains('|') {
        return Err(bad("name / rig must not contain `|`"));
    }
    let arg = format!("{name}|{rig}");
    json_passthrough(call_peer(&state, "company.bootstrap_founder", arg.as_bytes()).await?)
}

#[derive(Debug, Deserialize, Default)]
pub struct PinRequest {
    #[serde(default)]
    pub pinned: bool,
}

/// `POST /v1/spine/briefs/:id/pin` — pin/unpin a Brief.
pub async fn pin_brief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PinRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{id}|{}", i32::from(req.pinned));
    call_peer(&state, "brief.pin", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize)]
pub struct CommentRequest {
    pub author: String,
    pub text: String,
}

/// `POST /v1/spine/briefs/:id/comment` — comment on a Brief.
pub async fn comment_brief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CommentRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if req.author.trim().is_empty() || req.text.trim().is_empty() {
        return Err(bad("author and text required"));
    }
    if req.author.contains('|') {
        return Err(bad("author must not contain `|`"));
    }
    // `text` is the trailing field (splitn 3) so it may contain `|`.
    let arg = format!("{id}|{}|{}", req.author, req.text);
    call_peer(&state, "brief.comment", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize)]
pub struct SetFieldRequest {
    pub field: String,
    #[serde(default)]
    pub value: String,
}

/// `POST /v1/spine/briefs/:id/set` — set a spine field
/// (assignee/reviewer/priority/mandate/campaign). Empty value clears it
/// (where the field allows).
pub async fn set_field(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetFieldRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if !["assignee", "reviewer", "priority", "mandate", "campaign"].contains(&req.field.as_str()) {
        return Err(bad(
            "field must be assignee/reviewer/priority/mandate/campaign",
        ));
    }
    // `value` is the trailing wire field so it may contain `|`.
    let arg = format!("{id}|{}|{}", req.field, req.value);
    call_peer(&state, "brief.set", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize, Default)]
pub struct DueRequest {
    /// Unix seconds; null/omitted clears the due date.
    #[serde(default)]
    pub due_at: Option<i64>,
}

/// `POST /v1/spine/briefs/:id/due` — set/clear a Brief's due date.
pub async fn set_due(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DueRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = match req.due_at {
        Some(v) => format!("{id}|{v}"),
        None => format!("{id}|"),
    };
    call_peer(&state, "brief.set_due", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize)]
pub struct RelationRequest {
    /// The other Brief's task id (a blocker, or a child Sub-brief).
    pub other: String,
}

/// `POST /v1/spine/briefs/:id/snag` — record `id` blocked by `other`.
pub async fn add_snag(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RelationRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{id}|{}", req.other.trim());
    call_peer(&state, "brief.snag", arg.as_bytes()).await?;
    ok_json()
}

/// `POST /v1/spine/briefs/:id/unsnag` — clear the `id`→`other` Snag.
pub async fn remove_snag(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RelationRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{id}|{}", req.other.trim());
    call_peer(&state, "brief.unsnag", arg.as_bytes()).await?;
    ok_json()
}

/// `POST /v1/spine/briefs/:id/subbrief` — link `other` as a Sub-brief.
pub async fn add_subbrief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RelationRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{id}|{}", req.other.trim());
    call_peer(&state, "brief.subbrief", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize)]
pub struct CreateMandateRequest {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub owner_agent_id: Option<String>,
    #[serde(default)]
    pub parent_mandate_id: Option<String>,
}

/// `POST /v1/spine/mandates` — create a Mandate. Returns the id.
pub async fn create_mandate(
    State(state): State<AppState>,
    Json(req): Json<CreateMandateRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if req.title.trim().is_empty() {
        return Err(bad("title required"));
    }
    let opt = |o: &Option<String>| o.clone().unwrap_or_default();
    for (label, val) in [
        ("title", req.title.as_str()),
        ("description", &opt(&req.description)),
        ("owner_agent_id", &opt(&req.owner_agent_id)),
        ("parent_mandate_id", &opt(&req.parent_mandate_id)),
    ] {
        if val.contains('|') {
            return Err(bad(&format!("{label} must not contain `|`")));
        }
    }
    let arg = format!(
        "{}|{}|{}|{}",
        req.title,
        opt(&req.description),
        opt(&req.owner_agent_id),
        opt(&req.parent_mandate_id)
    );
    let body = call_peer(&state, "mandate.create", arg.as_bytes()).await?;
    json_id("mandate_id", &body)
}

// ── helpers ───────────────────────────────────────────────

/// A `200 {"ok":true}` for write actions with no return value.
fn ok_json() -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(br#"{"ok":true}"#.to_vec())
}

/// Wrap a raw id body as `{"<field>":"<id>"}`.
fn json_id(field: &str, body: &[u8]) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let id = String::from_utf8_lossy(body);
    let payload = serde_json::json!({ field: id.trim() }).to_string();
    json_passthrough(payload.into_bytes())
}

fn bad(msg: &str) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError { error: msg.into() }),
    )
}

/// Wrap a raw mesh body (already JSON for these capabilities) in a
/// `200 application/json` response. An empty body becomes `null`.
fn json_passthrough(body: Vec<u8>) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let payload = if body.is_empty() {
        b"null".to_vec()
    } else {
        body
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: format!("build response: {e}"),
                }),
            )
        })
}

/// Dial the coordinator and invoke `method` with `arg`, returning
/// the raw response body. Mirrors the `agent.*` routes' helper.
async fn call_peer(
    state: &AppState,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized".into(),
        }),
    ))?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 60);
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = mesh.call(DEFAULT_PEER, envelope).await.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_ascii_lowercase();
        let status = if lower.contains("unknown alias") || lower.contains("no peer") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_GATEWAY
        };
        (status, Json(ApiError { error: msg }))
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
    })?;
    match resp.res {
        ResponseResult::Ok(body) => Ok(body.to_vec()),
        ResponseResult::Err(env) => {
            let cause = env.cause;
            let lower = cause.to_ascii_lowercase();
            let status = if lower.contains("not found") {
                StatusCode::NOT_FOUND
            } else if env.kind == 5 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={cause}", env.kind),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from coordinator".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_passthrough_wraps_body_and_nulls_empty() {
        // Non-empty JSON body passes through with a JSON content type.
        let resp = json_passthrough(br#"{"total":3}"#.to_vec()).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );

        // An empty mesh body (e.g. "no labels") becomes JSON null, not
        // an empty 200 the browser can't parse.
        let empty = json_passthrough(Vec::new()).unwrap();
        assert_eq!(empty.status(), StatusCode::OK);
    }

    #[test]
    fn bad_is_a_400() {
        let (status, body) = bad("q required");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0.error, "q required");
    }

    #[test]
    fn parse_json_is_null_safe() {
        // Real JSON parses; empty and garbage degrade to null so the
        // composite payload is always well-formed.
        assert_eq!(parse_json(br#"[1,2]"#), serde_json::json!([1, 2]));
        assert_eq!(parse_json(b""), serde_json::Value::Null);
        assert_eq!(parse_json(b"not json"), serde_json::Value::Null);
    }

    #[test]
    fn normalize_clearance_decision_accepts_verbs_and_rejects_garbage() {
        assert_eq!(normalize_clearance_decision("approve"), Some("approved"));
        assert_eq!(normalize_clearance_decision("approved"), Some("approved"));
        assert_eq!(normalize_clearance_decision(" reject "), Some("rejected"));
        assert_eq!(normalize_clearance_decision("rejected"), Some("rejected"));
        // Anything else maps to None → the route returns 400.
        assert_eq!(normalize_clearance_decision("maybe"), None);
        assert_eq!(normalize_clearance_decision(""), None);
    }

    #[test]
    fn parse_clearance_lines_parses_tsv_and_drops_count() {
        let body = b"ap_1\tagt_x\tbrief.clearance_request\tneeds prod deploy\t1700\nap_2\tagt_y\tpayments\tspend $50\t1800\ncount=2\n";
        let got = parse_clearance_lines(body);
        let arr = got.as_array().unwrap();
        assert_eq!(arr.len(), 2, "the count= trailer must be dropped");
        assert_eq!(arr[0]["approval_id"], "ap_1");
        assert_eq!(arr[0]["agent_id"], "agt_x");
        assert_eq!(arr[0]["reason"], "needs prod deploy");
        assert_eq!(arr[1]["method"], "payments");
        // An empty body is an empty array, never null.
        assert_eq!(parse_clearance_lines(b""), serde_json::json!([]));
    }

    #[test]
    fn parse_event_lines_builds_array_and_skips_blanks() {
        // task.events is newline-delimited JSON objects; the composite
        // turns it into a real array and drops blank / unparseable lines.
        let body = b"{\"id\":1}\n\n{\"id\":2}\ngarbage\n";
        let got = parse_event_lines(body);
        assert_eq!(got, serde_json::json!([{"id":1},{"id":2}]));
        // An empty body is an empty array, never null.
        assert_eq!(parse_event_lines(b""), serde_json::json!([]));
    }

    /// Phase 2 Slice 2: the interim spine board is RETIRED — `/spine` is now a
    /// PERMANENT (308) redirect to the canonical React dashboard. The legacy
    /// board HTML + `legacy_page` handler are deleted; React owns the product
    /// surface. There is no longer any code path that serves the old board.
    #[tokio::test]
    async fn spine_page_permanently_redirects_to_dashboard() {
        let resp = page().await;
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(loc, "/dashboard", "must redirect to the React dashboard");
    }

    /// Build the full spine route table in isolation: matchit panics
    /// at `.route()` time on an overlapping/ambiguous pattern, so a
    /// clean construction here proves the routes are valid (the full
    /// app router is only built at server startup, not in tests).
    #[test]
    fn spine_routes_construct_without_conflict() {
        use axum::routing::{get, post};
        let _router: axum::Router<crate::config::AppState> = axum::Router::new()
            .route("/spine", get(page))
            .route("/v1/spine/guild", get(guild_counts))
            .route("/v1/spine/guild/detail", get(guild_detail))
            .route("/v1/spine/allowance/committed", get(allowance_committed))
            .route("/v1/spine/board", get(board_summary))
            .route("/v1/spine/board/:column", get(board_column))
            .route("/v1/spine/roster", get(roster_summary))
            .route("/v1/spine/mandates", get(mandates).post(create_mandate))
            .route("/v1/spine/mandates/search", get(mandate_search))
            .route("/v1/spine/mandates/:id/tree", get(mandate_tree))
            .route("/v1/spine/mandates/:id/briefs", get(mandate_briefs))
            .route(
                "/v1/spine/mandates/:id/team_plan",
                get(team_plan_latest).post(team_plan),
            )
            .route("/v1/spine/mandates/:id/team_readiness", get(team_readiness))
            .route("/v1/spine/mandates/:id/orchestrate", post(orchestrate))
            .route(
                "/v1/spine/mandates/:id/orchestration/latest",
                get(orchestration_latest),
            )
            .route("/v1/spine/briefs/search", get(brief_search))
            .route("/v1/spine/briefs/:id", get(brief_detail))
            .route("/v1/spine/briefs/:id/wakeups", get(brief_wakeups))
            .route("/v1/spine/desk/:agent", get(desk))
            .route("/v1/spine/by-label", get(by_label))
            .route("/v1/spine/overdue", get(overdue))
            .route("/v1/spine/blocked", get(blocked))
            .route("/v1/spine/stale", get(stale))
            .route("/v1/spine/unblocked", get(unblocked))
            .route("/v1/spine/unassigned", get(unassigned))
            .route("/v1/spine/keys/:agent", get(keys))
            .route("/v1/spine/assign_check", get(assign_check))
            .route("/v1/spine/clearances", get(clearances))
            .route(
                "/v1/spine/clearances/:approval_id/decide",
                post(decide_clearance),
            )
            .route("/v1/spine/inbox", get(inbox))
            .route("/v1/spine/briefs/:id/events", get(brief_events))
            .route("/v1/spine/briefs/:id/thread", get(brief_thread))
            .route("/v1/spine/briefs", post(create_brief))
            .route("/v1/spine/briefs/:id/move", post(move_brief))
            .route("/v1/spine/briefs/:id/pin", post(pin_brief))
            .route("/v1/spine/briefs/:id/comment", post(comment_brief))
            .route("/v1/spine/briefs/:id/due", post(set_due))
            .route("/v1/spine/briefs/:id/set", post(set_field))
            .route("/v1/spine/briefs/:id/snag", post(add_snag))
            .route("/v1/spine/briefs/:id/unsnag", post(remove_snag))
            .route("/v1/spine/briefs/:id/subbrief", post(add_subbrief));
    }
}
