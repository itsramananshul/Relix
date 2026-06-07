//! Prime guided driver (v1) — `company-model.md §5.4/§8.2` (the Action Center /
//! Board "next governed step", computed from live state and routed to existing
//! gates) focused onto a SINGLE Prime work session, plus `§12.5/§12.5B` (the
//! Prime planner + `prime.start`).
//!
//! This is the **bounded guide** surface. The opt-in autonomous Prime loop below
//! reuses this classifier / advance path on a timer, so manual and autonomous
//! routes share the same governed steps. Two capabilities:
//!
//!   - **`prime.next_step`** — READ-ONLY. Given a Prime proposal id OR a Mandate
//!     id, classify the one next governed step over live state: the proposal /
//!     strategy gate, the team plan + live readiness (hires / Clearances), the
//!     Brief board, and the run ledger. It mutates nothing.
//!
//!   - **`prime.advance`** — execute AT MOST ONE safe, explicitly-requested
//!     governed step. It re-reads state and runs the step ONLY when the requested
//!     `advance_action` still matches the current next step (else it refuses as
//!     stale with no side effects). The only auto-advanceable steps are
//!     `create_team_plan` (record a Team Plan from the Mandate's existing active
//!     crew — adopts active Operatives, mints **no** hires) and
//!     `orchestrate_assign_ready` (the existing `mandate.orchestrate` in
//!     `assign_ready` mode). It NEVER approves a strategy / hire / spawn / budget
//!     gate (those stay human) and NEVER runs a real adapter — `start_work` is
//!     deliberately routed to the existing explicit Prime **Start** button, not
//!     auto-advanced. Every step goes through the same governed handler + Keys as
//!     the manual route.
//!     The autonomous loop below is the separate timer that may call
//!     `prime.start` for already-approved, ready proposal work.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::coordinator::TaskStore;
use crate::nodes::coordinator::agent::handlers::{
    ReadinessView, autonomous_approve_spawn_clearance, brief_status_row, caller_is_operator,
    compute_readiness, handle_orchestrate, handle_prime_approve, handle_prime_start,
    handle_team_plan, internal, invalid, policy_denied,
};
use crate::nodes::coordinator::agent::prime;
use crate::nodes::coordinator::agent::store::{AgentStore, StandingApprovalMatch};
use crate::nodes::coordinator::spine::SpineStore;

/// The one-step advance keys the driver may execute on explicit operator
/// request. Strategy / hire / spawn / budget approvals are deliberately NOT
/// here — they stay human decisions.
const ADVANCE_CREATE_TEAM_PLAN: &str = "create_team_plan";
const ADVANCE_ORCHESTRATE: &str = "orchestrate_assign_ready";

// ── PRIME STANDING AUTHORITY (v1) ──────────────────────────────────────────
// The Board can grant the autonomous Prime loop bounded power to take specific
// governed APPROVAL actions on its behalf — but ONLY through an explicit
// `standing_approvals` row in the tenant, never from env alone. The grant is
// recorded against a SYNTHETIC authority subject (not a real Operative) and one
// of three narrow categories. This is "within powers you granted it", not a
// hidden bypass: with no standing row the loop leaves every approval gate to the
// human, exactly as before. (company-model standing-approval semantics.)

/// The synthetic standing-authority subject the Board grants bounded autonomous
/// Prime powers to. It is NOT a real Operative — it is a stable ASCII id used
/// only as the `agent_id` of `standing_approvals` rows that authorize the
/// autonomous Prime loop to take a governed approval action. Operators grant via
/// the existing `agent.standing_approval.create`
/// (`POST /v1/agents/__relix_autonomous_prime__/standing-approvals`) with one of
/// the categories below.
pub const AUTONOMOUS_PRIME_AUTHORITY: &str = "__relix_autonomous_prime__";

/// Standing-authority category: autonomous approval / materialization of a
/// PROPOSED Prime proposal (drives the existing `prime.approve` path).
pub const CATEGORY_PROPOSAL_APPROVE: &str = "prime.proposal.approve";
/// Standing-authority category: autonomous activation of a PENDING hire created
/// by Prime / company planning, onto the configured safe Rig.
pub const CATEGORY_HIRE_APPROVE: &str = "prime.hire.approve";
/// Standing-authority category: autonomous greenlight of a PENDING spawn
/// Clearance tied to Prime / company planning.
pub const CATEGORY_CLEARANCE_APPROVE: &str = "prime.clearance.approve";

/// The three standing-authority categories, in display order.
pub const STANDING_AUTHORITY_CATEGORIES: &[&str] = &[
    CATEGORY_PROPOSAL_APPROVE,
    CATEGORY_HIRE_APPROVE,
    CATEGORY_CLEARANCE_APPROVE,
];

/// Default safe Rig the autonomous hire-approve binds when
/// `RELIX_AUTONOMOUS_PRIME_HIRE_RIG` is unset — the safe-local `echo` built-in.
pub const DEFAULT_AUTONOMOUS_HIRE_RIG: &str = "echo";

// ── PRIME RUNTIME AUTONOMY SWITCH (v1) ──────────────────────────────────────
// The autonomous Prime *loop* (layer (a) above) was previously gated only by
// the boot-time env `RELIX_AUTONOMOUS_PRIME`. The runtime switch lets an
// operator turn the loop ON/OFF per Guild from the product at runtime — no
// restart, no env edit — persisted in the coordinator's SpineStore. This is
// emphatically NOT an approval bypass: turning the loop ON only wakes the
// driver; each governed approval still requires its own live standing grant
// (the categories above), and even the approved-work driver still goes through
// the same governed handlers + budget hard-stop. The env var stays a GLOBAL
// boot override: env ON ⇒ effective ON for every Guild (and the runtime OFF
// control can only clear the persisted row, not override env until restart);
// env OFF/unset ⇒ the persisted per-tenant setting decides.

/// SpineStore `runtime_settings.key` for the per-Guild autonomous-Prime loop
/// toggle. Generic table, one exposed key today.
pub const RUNTIME_KEY_AUTONOMOUS_PRIME: &str = "autonomous_prime_enabled";

/// The effective autonomous-Prime state for one Guild, given the global env
/// override and the persisted per-tenant runtime setting. Pure + testable.
/// Returns `(effective_enabled, source)` where `source` is `"env"` (env
/// override wins), `"runtime"` (persisted tenant setting on), or `"off"`.
pub fn effective_autonomy(env_enabled: bool, runtime_enabled: bool) -> (bool, &'static str) {
    if env_enabled {
        (true, "env")
    } else if runtime_enabled {
        (true, "runtime")
    } else {
        (false, "off")
    }
}

/// What the dormant autonomous-Prime watcher should drive on a tick. Pure +
/// testable so the controller loop carries no policy of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutonomyDrive {
    /// Nothing to do this tick (env off and no Guild has the runtime toggle on).
    Dormant,
    /// Env override is ON — drive ALL Guilds (`autonomous_prime_tick(tenant=None)`),
    /// exactly the legacy behaviour.
    AllGuilds,
    /// Env off but these specific Guilds have the runtime toggle on — drive each
    /// under its OWN Guild (`tenant=Some(g)`), never a Guild whose toggle is off.
    Tenants(Vec<String>),
}

/// Decide what the watcher drives this tick. The env override takes precedence
/// (drive all Guilds); otherwise drive only the Guilds whose persisted runtime
/// setting is on; an empty enabled set is dormant. A Guild whose runtime
/// setting is off is NEVER driven unless the env override is on.
pub fn plan_autonomy_drive(
    env_enabled: bool,
    runtime_enabled_tenants: Vec<String>,
) -> AutonomyDrive {
    if env_enabled {
        AutonomyDrive::AllGuilds
    } else if runtime_enabled_tenants.is_empty() {
        AutonomyDrive::Dormant
    } else {
        AutonomyDrive::Tenants(runtime_enabled_tenants)
    }
}

/// Whole seconds since the epoch — standing approvals store `expires_at` /
/// compare `now` in **seconds** (`store::unix_now`), so a standing check must
/// pass seconds, not the millisecond clock the budget gate uses.
fn now_secs_from_ms(now_ms: i64) -> i64 {
    now_ms.div_euclid(1000)
}

/// A standing-authority match for the synthetic Prime authority in `tenant`.
fn authority_match<'a>(
    tenant: &'a str,
    category: &'a str,
    now_secs: i64,
) -> StandingApprovalMatch<'a> {
    StandingApprovalMatch {
        agent_id: AUTONOMOUS_PRIME_AUTHORITY,
        category,
        method: "",
        task_id: None,
        session_id: None,
        workspace_path: None,
        tenant_id: Some(tenant),
        estimated_cost_micros: 0,
        now: now_secs,
    }
}

/// Is a standing authority for `category` currently active in `tenant`?
/// Gate-only (does not consume); a missing/expired/exhausted grant reads false.
fn standing_active(agent_store: &AgentStore, tenant: &str, category: &str, now_secs: i64) -> bool {
    agent_store
        .has_active_standing_for(authority_match(tenant, category, now_secs))
        .unwrap_or(false)
}

/// Consume ONE call of the active standing authority for `category` in `tenant`
/// after an autonomous action actually succeeded. A bounded grant
/// (`max_calls`/`max_cost`) is decremented; an unlimited grant returns `Some`
/// without decrementing (existing `consume_active_standing_for` semantics). Best
/// effort — a consume miss never undoes the action already taken.
fn consume_standing(
    agent_store: &AgentStore,
    tenant: &str,
    category: &str,
    now_secs: i64,
) -> Option<String> {
    agent_store
        .consume_active_standing_for(authority_match(tenant, category, now_secs))
        .ok()
        .flatten()
}

/// Wire arg for `prime.next_step`: exactly one of `proposal_id` / `mandate_id`.
#[derive(Debug, Default, Deserialize)]
struct TargetArgs {
    #[serde(default)]
    proposal_id: Option<String>,
    #[serde(default)]
    mandate_id: Option<String>,
}

/// Wire arg for `prime.advance`: the exact action to run. The target
/// (`proposal_id` / `mandate_id`) is re-parsed from the same args by
/// [`compute_next_step`] (via [`TargetArgs`]), so it is not duplicated here —
/// serde ignores those extra fields.
#[derive(Debug, Deserialize)]
struct AdvanceArgs {
    action: String,
}

/// The structured next step — the read-only verdict the dashboard renders.
pub(crate) struct NextStep {
    phase: &'static str,
    label: String,
    reason: String,
    /// The existing governed HTTP route the operator (or the driver) uses.
    route: String,
    /// The mesh capability backing that route.
    action_api: String,
    /// True only for a step the driver may execute via `prime.advance`.
    can_advance: bool,
    /// Stable advance key (`create_team_plan` / `orchestrate_assign_ready`),
    /// or `None` when the step is not auto-advanceable.
    advance_action: Option<&'static str>,
    proposal_id: Option<String>,
    mandate_id: Option<String>,
    plan_id: Option<String>,
    strategy_status: Option<String>,
    missing_roles: Vec<String>,
    pending_hires: Vec<Value>,
    pending_clearances: Vec<Value>,
    counts: BriefCounts,
}

impl NextStep {
    fn to_json(&self) -> Value {
        json!({
            "phase": self.phase,
            "label": self.label,
            "reason": self.reason,
            "route": self.route,
            "action_api": self.action_api,
            "can_advance": self.can_advance,
            "advance_action": self.advance_action,
            "proposal_id": self.proposal_id,
            "mandate_id": self.mandate_id,
            "plan_id": self.plan_id,
            "strategy_status": self.strategy_status,
            "missing_roles": self.missing_roles,
            "pending_hires": self.pending_hires,
            "pending_clearances": self.pending_clearances,
            "counts": self.counts.to_json(),
        })
    }
}

/// Brief-board roll-up over a fixed set of Brief ids, reusing the SAME bucketing
/// as `prime.status` (`brief_status_row`) so the driver and the Shift Room never
/// disagree.
#[derive(Default, Clone)]
struct BriefCounts {
    total: i64,
    running: i64,
    done: i64,
    blocked: i64,
    needs_review: i64,
    refused: i64,
    failed: i64,
    ready: i64,
    unassigned: i64,
    not_ready: i64,
    missing: i64,
}

impl BriefCounts {
    fn to_json(&self) -> Value {
        json!({
            "total_briefs": self.total,
            "running": self.running,
            "done": self.done,
            "blocked": self.blocked,
            "needs_review": self.needs_review,
            "refused": self.refused,
            "failed": self.failed,
            "ready": self.ready,
            "unassigned": self.unassigned,
            "not_ready": self.not_ready,
            "missing": self.missing,
        })
    }
}

/// Bucket each Brief id exactly as `prime.status` does. Tenant-scoped reads.
fn brief_counts(
    agent_store: &AgentStore,
    task_store: &TaskStore,
    tenant: &str,
    brief_ids: &[String],
) -> BriefCounts {
    let ready_set: std::collections::HashSet<String> = task_store
        .list_ready_briefs(500)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.task_id)
        .collect();
    let mut c = BriefCounts {
        total: brief_ids.len() as i64,
        ..BriefCounts::default()
    };
    for id in brief_ids {
        let row = brief_status_row(agent_store, task_store, tenant, id, ready_set.contains(id));
        match row.bucket {
            "running" => c.running += 1,
            "done" => c.done += 1,
            "blocked" => c.blocked += 1,
            "needs_review" => c.needs_review += 1,
            "refused" => c.refused += 1,
            "failed" => c.failed += 1,
            "ready" => c.ready += 1,
            "unassigned" => c.unassigned += 1,
            "missing" => c.missing += 1,
            _ => c.not_ready += 1,
        }
    }
    c
}

/// The distinct canonical **work** roles of the Guild's currently-active
/// Operatives. `create_team_plan` passes these to the existing team-plan logic
/// so it staffs the team from the crew you already have (adopts active
/// Operatives, mints no hires). Leadership roles (founder/prime/planner) are not
/// work tracks (`prime::try_canon_role` returns `None`) and never appear here.
fn active_crew_roles(agent_store: &AgentStore, tenant: &str) -> Vec<&'static str> {
    let mut roles: Vec<&'static str> = Vec::new();
    for p in agent_store
        .list_active_for_tenant(tenant)
        .unwrap_or_default()
    {
        if let Some(canon) = prime::try_canon_role(&p.role)
            && !roles.contains(&canon)
        {
            roles.push(canon);
        }
    }
    roles
}

/// Compute the next governed step for a proposal or a mandate. Returns
/// `Err(HandlerOutcome)` for an invalid arg / not-found target so a caller can
/// return it verbatim.
fn compute_next_step(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    ctx: &InvocationCtx,
) -> Result<NextStep, HandlerOutcome> {
    let args: TargetArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return Err(invalid(format!("prime.next_step: bad args: {e}"))),
    };
    let tenant = ctx.tenant_id_or_default();

    if let Some(pid) = args
        .proposal_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let row = match spine_store.get_prime_proposal(tenant, pid) {
            Ok(Some(r)) => r,
            Ok(None) => return Err(invalid(format!("proposal not found: {pid}"))),
            Err(e) => return Err(internal(format!("prime.next_step load: {e}"))),
        };
        // Proposal not yet approved → the approval gate (human).
        if row.status != "approved" {
            return Ok(proposal_pre_approval_step(&row));
        }
        // Approved → it carries the Mandate + its created Briefs.
        if row.mandate_id.is_empty() {
            return Ok(unknown_step(Some(pid.to_string()), None));
        }
        let brief_ids: Vec<String> =
            serde_json::from_str(&row.created_brief_ids).unwrap_or_default();
        return classify_mandate(
            agent_store,
            spine_store,
            task_store,
            tenant,
            Some(pid.to_string()),
            &row.mandate_id,
            brief_ids,
        );
    }

    if let Some(mid) = args
        .mandate_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        // Tenant-gate: an unknown / cross-Guild Mandate reads as not-found.
        match spine_store.get_mandate_for_tenant(mid, tenant) {
            Ok(Some(_)) => {}
            Ok(None) => return Err(invalid(format!("mandate not found: {mid}"))),
            Err(e) => return Err(internal(format!("prime.next_step mandate: {e}"))),
        }
        let brief_ids: Vec<String> = task_store
            .list_briefs_by_mandate(mid, 500)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.task_id)
            .collect();
        return classify_mandate(
            agent_store,
            spine_store,
            task_store,
            tenant,
            None,
            mid,
            brief_ids,
        );
    }

    Err(invalid(
        "prime.next_step: proposal_id or mandate_id required".into(),
    ))
}

/// The next step for a proposal that has not been approved yet.
fn proposal_pre_approval_step(
    row: &crate::nodes::coordinator::spine::store::PrimeProposalRow,
) -> NextStep {
    let pid = Some(row.proposal_id.clone());
    if row.status == "rejected" {
        return NextStep {
            phase: "blocked",
            label: "Proposal was rejected".into(),
            reason: "This proposal was rejected. Describe the goal again to get a fresh plan."
                .into(),
            route: "POST /v1/spine/prime/propose".into(),
            action_api: "prime.propose".into(),
            can_advance: false,
            advance_action: None,
            proposal_id: pid,
            mandate_id: None,
            plan_id: None,
            strategy_status: None,
            missing_roles: Vec::new(),
            pending_hires: Vec::new(),
            pending_clearances: Vec::new(),
            counts: BriefCounts::default(),
        };
    }
    NextStep {
        phase: "needs_approval",
        label: "Approve & create".into(),
        reason: "Prime has proposed a governed plan. Approve it to create the Mandate, \
                 Briefs, crew assignments, and pending hire requests. Nothing is created \
                 or run until you approve."
            .into(),
        route: "POST /v1/spine/prime/approve".into(),
        action_api: "prime.approve".into(),
        can_advance: false,
        advance_action: None,
        proposal_id: pid,
        mandate_id: None,
        plan_id: None,
        strategy_status: None,
        missing_roles: Vec::new(),
        pending_hires: Vec::new(),
        pending_clearances: Vec::new(),
        counts: BriefCounts::default(),
    }
}

fn unknown_step(proposal_id: Option<String>, mandate_id: Option<String>) -> NextStep {
    NextStep {
        phase: "unknown",
        label: "No clear next step".into(),
        reason: "The work session has no obvious next governed step — inspect it on the board."
            .into(),
        route: "/briefs".into(),
        action_api: String::new(),
        can_advance: false,
        advance_action: None,
        proposal_id,
        mandate_id,
        plan_id: None,
        strategy_status: None,
        missing_roles: Vec::new(),
        pending_hires: Vec::new(),
        pending_clearances: Vec::new(),
        counts: BriefCounts::default(),
    }
}

/// Classify the next governed step for an approved Mandate (proposal- or
/// strategy-origin). `proposal_id` is `Some` only when reached through a Prime
/// proposal (so the ready-work route is the explicit Prime **Start** button).
#[allow(clippy::too_many_lines)]
fn classify_mandate(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    tenant: &str,
    proposal_id: Option<String>,
    mandate_id: &str,
    brief_ids: Vec<String>,
) -> Result<NextStep, HandlerOutcome> {
    let r: ReadinessView = compute_readiness(agent_store, spine_store, tenant, mandate_id)
        .map_err(|e| internal(format!("prime.next_step readiness: {e}")))?;
    let strategy = spine_store
        .strategy_status(tenant, mandate_id)
        .unwrap_or(None);
    let approved = strategy.as_deref() == Some("approved");
    let counts = brief_counts(agent_store, task_store, tenant, &brief_ids);
    let plan_id = r.plan.as_ref().map(|p| p.plan_id.clone());

    let mid = mandate_id.to_string();
    // A small builder so every arm stays consistent on the shared fields.
    let base = |phase: &'static str,
                label: &str,
                reason: String,
                route: &str,
                api: &str,
                can_advance: bool,
                advance_action: Option<&'static str>|
     -> NextStep {
        NextStep {
            phase,
            label: label.into(),
            reason,
            route: route.into(),
            action_api: api.into(),
            can_advance,
            advance_action,
            proposal_id: proposal_id.clone(),
            mandate_id: Some(mid.clone()),
            plan_id: plan_id.clone(),
            strategy_status: strategy.clone(),
            missing_roles: r.missing_roles.clone(),
            pending_hires: r.pending_hires.clone(),
            pending_clearances: r.pending_clearances.clone(),
            counts: counts.clone(),
        }
    };

    // The ready-work route differs by entry: a Prime proposal starts through the
    // explicit `prime.start` button; a bare Mandate runs its Briefs per-Brief.
    let (start_route, start_api) = if proposal_id.is_some() {
        ("POST /v1/spine/prime/start", "prime.start")
    } else {
        ("POST /v1/spine/briefs/:id/run", "brief.run")
    };

    // Strategy gate (human) — only blocks the strategy-origin flow before
    // approval. A proposal-origin Mandate has no strategy gate row but is already
    // planned, so it skips this and is classified by readiness below.
    if !approved && !r.planned {
        return Ok(match strategy.as_deref() {
            Some("proposed") => base(
                "needs_approval",
                "Approve the Mandate strategy",
                "A strategy is proposed for this Mandate. Approve it to unlock team \
                 planning and orchestration."
                    .into(),
                "POST /v1/spine/mandates/:id/strategy/approve",
                "mandate.strategy.approve",
                false,
                None,
            ),
            Some("rejected") => base(
                "blocked",
                "Strategy rejected",
                "The Mandate strategy was rejected. Propose a new strategy to continue.".into(),
                "POST /v1/spine/mandates/:id/strategy/propose",
                "mandate.strategy.propose",
                false,
                None,
            ),
            _ => base(
                "needs_approval",
                "Propose & approve a strategy",
                "This Mandate has no approved strategy yet. Propose one, then approve it, \
                 before planning a team."
                    .into(),
                "POST /v1/spine/mandates/:id/strategy/propose",
                "mandate.strategy.propose",
                false,
                None,
            ),
        });
    }

    // Governance gates first (human): pending Clearances, then pending hires.
    if !r.pending_clearances.is_empty() {
        return Ok(base(
            "needs_hire_approval",
            "Greenlight pending Clearances",
            format!(
                "{} pending Clearance(s) must be greenlit to activate the hires. This is a \
                 human approval — the driver will not auto-approve it.",
                r.pending_clearances.len()
            ),
            "POST /v1/spine/clearances/:id/decide",
            "coord.approval.decide",
            false,
            None,
        ));
    }
    if !r.pending_hires.is_empty() {
        return Ok(base(
            "needs_hire_approval",
            "Approve pending hires",
            format!(
                "{} pending hire(s) need approval before they can run. This is a human \
                 approval — the driver will not auto-approve it.",
                r.pending_hires.len()
            ),
            "POST /v1/agents/:id/approve-hire",
            "agent.approve_hire",
            false,
            None,
        ));
    }

    // No Team Plan yet — and (we are past the strategy gate, so) strategy is
    // approved. The driver may record one from the existing active crew.
    if !r.planned {
        return Ok(base(
            "needs_team_plan",
            "Plan the team",
            "No Team Plan exists for this approved Mandate. The driver can record one from \
             your active crew (it adopts active Operatives and files no hires)."
                .into(),
            "POST /v1/spine/mandates/:id/team_plan",
            "mandate.team_plan",
            approved,
            Some(ADVANCE_CREATE_TEAM_PLAN),
        ));
    }

    // Team is ready — orchestrate or run.
    if r.readiness == "ready" {
        if counts.total == 0 {
            return Ok(base(
                "needs_orchestration",
                "Create & assign the Brief tree",
                "The team is ready and no Briefs exist yet. The driver can create and \
                 assign the Brief tree through the existing orchestration gate."
                    .into(),
                "POST /v1/spine/mandates/:id/orchestrate",
                "mandate.orchestrate",
                approved,
                Some(ADVANCE_ORCHESTRATE),
            ));
        }
        if counts.unassigned > 0 {
            return Ok(base(
                "needs_orchestration",
                "Assign ready Briefs",
                format!(
                    "{} Brief(s) are unassigned and the team is ready. The driver can assign \
                     them through the existing orchestration gate.",
                    counts.unassigned
                ),
                "POST /v1/spine/mandates/:id/orchestrate",
                "mandate.orchestrate",
                approved,
                Some(ADVANCE_ORCHESTRATE),
            ));
        }
        if counts.ready > 0 {
            return Ok(base(
                "ready_to_start",
                "Start the ready Briefs",
                format!(
                    "{} Brief(s) are assigned, unblocked, and ready to run as Shifts. Use the \
                     explicit Start control, or enable autonomous Prime to start approved ready \
                     proposal work through the same prime.start path.",
                    counts.ready
                ),
                start_route,
                start_api,
                false,
                None,
            ));
        }
        if counts.running > 0 {
            return Ok(base(
                "running_or_done",
                "Shifts running",
                format!(
                    "{} Shift(s) are running — inspect progress.",
                    counts.running
                ),
                "/runs",
                "brief.runs",
                false,
                None,
            ));
        }
        if counts.needs_review > 0 {
            return Ok(base(
                "running_or_done",
                "Review completed Shifts",
                format!(
                    "{} completed Shift(s) are awaiting review → apply.",
                    counts.needs_review
                ),
                "/runs",
                "brief.runs",
                false,
                None,
            ));
        }
        if counts.failed + counts.refused > 0 {
            return Ok(base(
                "blocked",
                "Shifts need attention",
                format!(
                    "{} Shift(s) failed or were refused — inspect the run and recover.",
                    counts.failed + counts.refused
                ),
                "/runs",
                "brief.runs",
                false,
                None,
            ));
        }
        if counts.blocked > 0 {
            return Ok(base(
                "blocked",
                "Briefs blocked",
                format!("{} Brief(s) are blocked on a dependency.", counts.blocked),
                "/briefs",
                "brief.detail",
                false,
                None,
            ));
        }
        if counts.total > 0 && counts.done == counts.total {
            return Ok(base(
                "running_or_done",
                "All Briefs done",
                "Every Brief in this session is done.".into(),
                "/briefs",
                "brief.detail",
                false,
                None,
            ));
        }
        return Ok(unknown_step(proposal_id, Some(mid)));
    }

    // Planned but staffing: a role with no identity needs a human decision.
    if !r.missing_roles.is_empty() {
        return Ok(base(
            "needs_team_plan",
            "Staff missing roles",
            format!(
                "{} role(s) have no Operative. Staff them with an identity through the team-plan \
                 route — the driver will not pick who to hire.",
                r.missing_roles.len()
            ),
            "POST /v1/spine/mandates/:id/team_plan",
            "mandate.team_plan",
            false,
            None,
        ));
    }

    // Planned but empty (no crew yet): the driver can (re)plan from active crew.
    Ok(base(
        "needs_team_plan",
        "Add roles to the team",
        "The Team Plan has no active crew. The driver can re-plan from your active \
         Operatives (adopts active crew, files no hires)."
            .into(),
        "POST /v1/spine/mandates/:id/team_plan",
        "mandate.team_plan",
        approved,
        Some(ADVANCE_CREATE_TEAM_PLAN),
    ))
}

fn ok_json(body: &Value) -> HandlerOutcome {
    match serde_json::to_vec(body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("prime driver encode: {e}")),
    }
}

/// `prime.next_step` — READ-ONLY. Classify the next governed step for a Prime
/// proposal or a Mandate. Tenant-scoped; mutates nothing. Arg (JSON):
/// `{"proposal_id":"…"}` or `{"mandate_id":"…"}`.
pub fn handle_prime_next_step(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    match compute_next_step(agent_store, spine_store, task_store, ctx) {
        Ok(step) => ok_json(&step.to_json()),
        Err(out) => out,
    }
}

/// `prime.advance` — execute AT MOST ONE safe, explicitly-requested governed
/// step. Re-reads state and runs the step ONLY when the requested
/// `advance_action` still matches the current next step (else refuses as stale
/// with NO side effects). Arg (JSON):
/// `{"proposal_id"|"mandate_id":"…","action":"create_team_plan"|"orchestrate_assign_ready"}`.
/// Governance is unchanged — the step runs through the existing handler with the
/// caller's identity + Keys.
pub fn handle_prime_advance(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: AdvanceArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid(format!("prime.advance: bad args: {e}")),
    };
    let requested = args.action.trim().to_string();
    if requested != ADVANCE_CREATE_TEAM_PLAN && requested != ADVANCE_ORCHESTRATE {
        return invalid(format!("prime.advance: unknown action `{requested}`"));
    }

    // Re-read state. `compute_next_step` parses `proposal_id`/`mandate_id` out of
    // the SAME ctx args (the extra `action` field is ignored).
    let step = match compute_next_step(agent_store, spine_store, task_store, ctx) {
        Ok(s) => s,
        Err(out) => return out,
    };

    // Stale guard: refuse (no side effects) unless the requested action is STILL
    // the current advanceable next step. The bridge maps this onto a 409.
    if !step.can_advance || step.advance_action != Some(requested.as_str()) {
        let body = json!({
            "advanced": false,
            "refused": "stale_action",
            "requested_action": requested,
            "reason": "The requested step is no longer the current next step. Re-read \
                       prime.next_step and try again.",
            "next_step": step.to_json(),
        });
        return ok_json(&body);
    }

    let tenant = ctx.tenant_id_or_default();
    let Some(mandate_id) = step.mandate_id.clone() else {
        return internal("prime.advance: next step has no mandate".into());
    };

    // Dispatch EXACTLY ONE governed step through the existing handler, carrying
    // the caller's identity + Keys (governance unchanged). Build a sub-ctx that
    // only swaps the args; never elevate the caller.
    let mut sub = ctx.clone();
    let result = match requested.as_str() {
        ADVANCE_CREATE_TEAM_PLAN => {
            // Plan from the existing active crew (adopts active Operatives,
            // mints no hires). Roles are the distinct work roles already on the
            // active roster; an empty roster records an inert plan shell.
            let roles = active_crew_roles(agent_store, tenant).join(",");
            sub.args = format!("{mandate_id}|Prime guided driver|{roles}").into_bytes();
            handle_team_plan(agent_store, spine_store, &sub)
        }
        // `orchestrate_assign_ready` → the existing orchestration gate in
        // assign_ready mode (strategy + ready-team gated; idempotent tree).
        _ => {
            sub.args = format!("{mandate_id}|assign_ready").into_bytes();
            handle_orchestrate(task_store, agent_store, spine_store, &sub)
        }
    };
    let result_json: Value = match result {
        HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap_or(Value::Null),
        // Propagate a governance refusal / error honestly (no fake success).
        err @ HandlerOutcome::Err(_) => return err,
    };

    // Recompute the next step so the caller sees where the session is now.
    let after = compute_next_step(agent_store, spine_store, task_store, ctx)
        .ok()
        .map(|s| s.to_json());
    let body = json!({
        "advanced": true,
        "action": requested,
        "mandate_id": mandate_id,
        "result": result_json,
        "next_step": after,
    });
    ok_json(&body)
}

/// The configured autonomous hire Rig: `RELIX_AUTONOMOUS_PRIME_HIRE_RIG`,
/// trimmed, default [`DEFAULT_AUTONOMOUS_HIRE_RIG`] when unset/blank. The raw
/// value is passed through unvalidated on purpose — the tick validates it
/// against the known-Rig allowlist and **refuses/skips** a hire rather than
/// silently binding a bad Rig, so a typo is surfaced (left pending) instead of
/// quietly downgraded.
pub fn configured_autonomous_hire_rig() -> String {
    std::env::var("RELIX_AUTONOMOUS_PRIME_HIRE_RIG")
        .ok()
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_AUTONOMOUS_HIRE_RIG)
        .to_string()
}

/// `prime.standing_authority` — READ-ONLY. The Prime standing-authority state
/// for the caller's Guild: whether each of the three categories is currently
/// active (a non-expired, non-exhausted `standing_approvals` row exists for the
/// synthetic authority subject in this tenant), plus the synthetic authority id,
/// the grantable categories, and the configured autonomous hire Rig. Mutates
/// nothing; surfaces NO secret. The grant/revoke routes are the existing
/// `agent.standing_approval.*` (`/v1/agents/:id/standing-approvals`).
pub fn handle_prime_standing_authority(
    agent_store: &AgentStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let tenant = ctx.tenant_id_or_default();
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let driver_enabled = crate::nodes::coordinator::heartbeat::parse_autonomous_prime_enabled(
        std::env::var("RELIX_AUTONOMOUS_PRIME").ok().as_deref(),
    );
    let hire_rig = configured_autonomous_hire_rig();
    let hire_rig_valid = crate::rig::is_known_rig(&hire_rig);
    let descriptions = [
        "Autonomously approve/materialize a proposed Prime proposal through the existing prime.approve path.",
        "Autonomously activate a pending hire created by Prime/company planning, bound to the configured safe Rig.",
        "Autonomously greenlight a pending spawn Clearance tied to Prime/company planning.",
    ];
    let categories: Vec<Value> = STANDING_AUTHORITY_CATEGORIES
        .iter()
        .zip(descriptions.iter())
        .map(|(cat, desc)| {
            json!({
                "category": cat,
                "active": standing_active(agent_store, tenant, cat, now_secs),
                "description": desc,
            })
        })
        .collect();
    let body = json!({
        "authority_id": AUTONOMOUS_PRIME_AUTHORITY,
        // Legacy env-derived field retained for compatibility. The authoritative
        // effective runtime/env loop state is `prime.autonomy_state`.
        "driver_enabled": driver_enabled,
        "hire_rig": hire_rig,
        "hire_rig_valid": hire_rig_valid,
        "categories": categories,
        "note": "These are standing approvals granted to the synthetic Prime authority, not loop toggles. \
                 The runtime toggle or RELIX_AUTONOMOUS_PRIME env override only wakes the loop; each category \
                 above acts only when a standing-approval row exists for this Guild. Grant/revoke via \
                 POST/DELETE /v1/agents/__relix_autonomous_prime__/standing-approvals.",
    });
    ok_json(&body)
}

/// Wire arg for `prime.autonomy_set`: the desired runtime ON/OFF state.
#[derive(Debug, Deserialize)]
struct AutonomySetArgs {
    enabled: bool,
}

/// Read the live env-derived autonomous-Prime knobs (enabled / max / interval /
/// hire Rig). Centralised so the read capability and the bridge surface one set
/// of figures.
fn env_autonomy_knobs() -> (bool, usize, u64, String) {
    let env_enabled = crate::nodes::coordinator::heartbeat::parse_autonomous_prime_enabled(
        std::env::var("RELIX_AUTONOMOUS_PRIME").ok().as_deref(),
    );
    let max = crate::nodes::coordinator::heartbeat::parse_autonomous_prime_max(
        std::env::var("RELIX_AUTONOMOUS_PRIME_MAX").ok().as_deref(),
    );
    let interval = std::env::var("RELIX_AUTONOMOUS_PRIME_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(30);
    (env_enabled, max, interval, configured_autonomous_hire_rig())
}

/// Build the autonomy-state JSON for the caller's Guild from the persisted
/// runtime setting + the env override. Shared by the read capability and the
/// mutation's response so a toggle returns the exact same shape a fresh read
/// would. `runtime_enabled` is the persisted per-tenant value (default off).
fn autonomy_state_json(runtime_enabled: bool) -> Value {
    let (env_enabled, max, interval, hire_rig) = env_autonomy_knobs();
    let (effective_enabled, source) = effective_autonomy(env_enabled, runtime_enabled);
    json!({
        "runtime_enabled": runtime_enabled,
        "env_enabled": env_enabled,
        "effective_enabled": effective_enabled,
        "source": source,
        "autonomous_prime_max": max,
        "autonomous_prime_interval_secs": interval,
        "hire_rig": hire_rig,
        // The env var is a GLOBAL boot override: while it is set the loop runs
        // for every Guild and the runtime OFF control can only clear the
        // persisted row (effective stays ON until the env is changed + restart).
        "env_override": env_enabled,
        // Honest safety note: turning the loop ON is NOT an approval bypass.
        "note": "Turning autonomous Prime ON only wakes the loop over already-approved work. \
                 It never approves a governed gate on its own — each approval category still \
                 requires a live standing grant (see Prime standing authority). When env \
                 RELIX_AUTONOMOUS_PRIME is set it is a global override: the loop runs for every \
                 Guild and this runtime toggle cannot fully disable it until the env is changed.",
    })
}

/// `prime.autonomy_state` — READ-ONLY. The effective autonomous-Prime loop state
/// for the caller's Guild: the persisted runtime toggle, the env override, the
/// effective state + its source, plus the live max/interval/hire-Rig knobs and
/// the standing-grant caveat. Tenant-scoped; mutates nothing; surfaces no
/// secret.
pub fn handle_prime_autonomy_state(
    spine_store: &SpineStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let tenant = ctx.tenant_id_or_default();
    let runtime_enabled = spine_store
        .get_runtime_setting_bool(tenant, RUNTIME_KEY_AUTONOMOUS_PRIME)
        .unwrap_or(None)
        .unwrap_or(false);
    ok_json(&autonomy_state_json(runtime_enabled))
}

/// `prime.autonomy_set` — turn the autonomous-Prime loop ON/OFF for the caller's
/// Guild at runtime (no restart). Arg (JSON): `{"enabled": bool}`. Persists the
/// tenant-scoped runtime setting in the SpineStore. ROLE-GATED to the
/// Founder/Board (operator/admin) — a normal worker subject can never flip it.
/// This is **not** an approval bypass: even ON, the loop only drives
/// already-approved work, and each governed approval still needs its own live
/// standing grant. When the env override is set, the persisted value is still
/// written (so it takes effect if env is later cleared) but the response's
/// `effective_enabled` honestly reflects that env keeps the loop ON.
pub fn handle_prime_autonomy_set(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    // Same admin gate as other Board-only runtime controls (agent.create etc.):
    // only an operator/admin caller may change a Guild's autonomy setting.
    if !caller_is_operator(ctx) {
        return policy_denied(
            "prime.autonomy_set is operator/admin-only — a worker subject cannot toggle \
             autonomous Prime"
                .to_string(),
        );
    }
    let args: AutonomySetArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid(format!("prime.autonomy_set: bad args: {e}")),
    };
    let tenant = ctx.tenant_id_or_default();
    let updated_by = ctx.caller.subject_id.to_string();
    if let Err(e) = spine_store.set_runtime_setting_bool(
        tenant,
        RUNTIME_KEY_AUTONOMOUS_PRIME,
        args.enabled,
        &updated_by,
    ) {
        return internal(format!("prime.autonomy_set persist: {e}"));
    }
    // Return the fresh state (the persisted value we just wrote + env override).
    ok_json(&autonomy_state_json(args.enabled))
}

// ─────────────────────────────────────────────────────────────────────────
// AUTONOMOUS PRIME DRIVER (v1) — opt-in, bounded (company-model §5.4/§8.2 the
// Action Center "next governed step"; §12.5/§12.5B the Prime planner + Start).
//
// This is the **loop** the guided driver was missing: instead of the operator
// clicking "Advance one step" over and over, a timer drives already-approved
// Prime work forward on its own. It is emphatically NOT "an AI CEO that does
// whatever it wants" — every action goes through the SAME governed handler the
// operator click uses, it advances ONLY the safe steps `prime.advance` already
// allows (`create_team_plan` / `orchestrate_assign_ready`) plus starting ready
// work for an already-approved proposal through the existing `prime.start`
// path, and it NEVER auto-approves a strategy / hire / spawn / budget /
// Clearance gate (those stay human). Bounded per tick, idempotent (each tick
// re-classifies, so team plans / orchestration trees / started Shifts never
// duplicate), and tenant-safe (each candidate is processed under its OWN Guild).
// ─────────────────────────────────────────────────────────────────────────

/// What one autonomous Prime tick did with one candidate (for logs + tests).
/// Durable provenance for an actual action lives in the Chronicle event the
/// handler / this driver writes; this is the in-memory tick summary.
#[derive(Debug, Clone)]
pub struct PrimeAutonomyRecord {
    /// The Guild the candidate (and its action) belongs to.
    pub tenant: String,
    /// `proposal` or `mandate`.
    pub target_kind: &'static str,
    /// The proposal_id or mandate_id processed.
    pub target_id: String,
    /// The resolved Mandate id (when known).
    pub mandate_id: Option<String>,
    /// The classified next-step phase (`needs_team_plan` / `needs_orchestration`
    /// / `ready_to_start` / `needs_approval` / …).
    pub phase: String,
    /// The action attempted: `create_team_plan` / `orchestrate_assign_ready` /
    /// `start` / `none`.
    pub action: &'static str,
    /// `advanced` / `started` / `skipped` / `blocked`.
    pub outcome: &'static str,
    /// A short, secret-free reason for the outcome.
    pub reason: String,
}

/// Build the synthetic **autonomous Prime** invocation context for `tenant`.
/// Role `operator` because the autonomous loop is the **Board's sovereign
/// automation** over already-approved work — exactly what the operator does by
/// clicking Advance / Start — so it takes the same sovereign path through the
/// spawn / assign Keys that the manual `prime.advance` / `prime.start` already
/// take. It grants NO new authority: the handlers' own gates (strategy approved,
/// ready team, no pending hires / Clearances, active assignee, Claim, adapter,
/// budget on the autonomous boundary) all still apply.
pub(crate) fn autonomous_prime_ctx(tenant: &str, args: Vec<u8>) -> InvocationCtx {
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};
    InvocationCtx {
        caller: VerifiedIdentity {
            subject_id: NodeId::from_pubkey(b"relix:autonomous-prime"),
            name: "autonomous-prime".into(),
            org_id: NodeId::from_pubkey(b"relix:org"),
            groups: vec![],
            role: "operator".into(),
            clearance: String::new(),
            bundle_id: [0; 32],
        },
        trace_id: TraceId::new(),
        request_id: RequestId::new(),
        args,
        tenant_id: Some(tenant.to_string()),
    }
}

/// Append ONE Chronicle event for an actual autonomous action onto the Mandate's
/// first (parent / orchestration-root) Brief — sparingly, only when a Brief
/// exists. No Brief yet (e.g. a team-plan before orchestration) → record-only,
/// no event, so an idle loop never spams the Chronicle.
fn chronicle_autonomous(task_store: &TaskStore, mandate_id: &str, event_type: &str, detail: &str) {
    if let Ok(briefs) = task_store.list_briefs_by_mandate(mandate_id, 1)
        && let Some(first) = briefs.first()
    {
        let _ = task_store.append_event(&first.task_id, event_type, detail);
    }
}

/// Pre-gate the autonomous **start** of an approved proposal's ready Briefs with
/// the SAME budget hard-stop the autonomous heartbeat applies per Brief
/// ([`heartbeat::dispatch_budget_admits`] — per-Operative Allowance + additive
/// Guild budget). `prime.start` itself is the sovereign manual path and takes no
/// budget gate; this re-imposes the gate at the **autonomous** boundary so the
/// loop never auto-starts an over-budget Brief. Conservative: if ANY currently-
/// ready Brief of the proposal is over budget, the whole autonomous start is
/// refused (the operator's manual Start stays sovereign; the heartbeat still
/// gates per Brief). When metrics/spine are unavailable the gate is inert
/// (allows), mirroring the heartbeat.
fn start_budget_admitted(
    task_store: &TaskStore,
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    metrics: Option<&crate::metrics::MetricsQuery>,
    tenant: &str,
    proposal_id: &str,
    now_ms: i64,
) -> Result<(), String> {
    let row = match spine_store.get_prime_proposal(tenant, proposal_id) {
        Ok(Some(r)) => r,
        // Can't read the proposal → don't fabricate a stop; let prime.start
        // classify (it is tenant-gated and refuses a non-approved proposal).
        _ => return Ok(()),
    };
    let created: Vec<String> = serde_json::from_str(&row.created_brief_ids).unwrap_or_default();
    if created.is_empty() {
        return Ok(());
    }
    let ready: std::collections::HashSet<String> = task_store
        .list_ready_briefs(500)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.task_id)
        .collect();
    for id in &created {
        if !ready.contains(id) {
            continue;
        }
        if let Ok(Some(card)) = task_store.brief_card(id)
            && let crate::nodes::coordinator::heartbeat::BudgetAdmission::Refuse { reason, .. } =
                crate::nodes::coordinator::heartbeat::dispatch_budget_admits(
                    &card,
                    task_store,
                    agent_store,
                    Some(spine_store),
                    metrics,
                    now_ms,
                )
        {
            return Err(reason);
        }
    }
    Ok(())
}

/// Process ONE autonomous candidate: classify its next governed step and, when
/// it is a safe auto-advanceable step (or a ready-to-start approved proposal),
/// execute exactly that one step through the existing governed handler. Counts a
/// real mutation against `actions` (so the tick stays bounded by `max`); a human
/// gate / already-running / done step records and acts on nothing.
#[allow(clippy::too_many_arguments)]
fn process_candidate(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    metrics: Option<&crate::metrics::MetricsQuery>,
    now_ms: i64,
    tenant: &str,
    kind: &'static str,
    target_id: &str,
    target: Value,
    actions: &mut usize,
    max: usize,
    hire_rig: &str,
) -> PrimeAutonomyRecord {
    let mk = |phase: String,
              action: &'static str,
              outcome: &'static str,
              reason: String,
              mandate_id: Option<String>|
     -> PrimeAutonomyRecord {
        PrimeAutonomyRecord {
            tenant: tenant.to_string(),
            target_kind: kind,
            target_id: target_id.to_string(),
            mandate_id,
            phase,
            action,
            outcome,
            reason,
        }
    };

    // Classify the one next governed step (READ-ONLY) under this candidate's
    // own tenant.
    let read_ctx = autonomous_prime_ctx(tenant, target.to_string().into_bytes());
    let step = match compute_next_step(agent_store, spine_store, task_store, &read_ctx) {
        Ok(s) => s,
        Err(_) => {
            return mk(
                "unknown".into(),
                "none",
                "skipped",
                "target not classifiable".into(),
                None,
            );
        }
    };
    let phase = step.phase.to_string();
    let mandate_id = step.mandate_id.clone();

    // (A) Safe auto-advance steps — create_team_plan / orchestrate_assign_ready
    // — through the SAME governed advance path the operator click uses (it
    // re-reads state + refuses a stale action with no side effects).
    if step.can_advance
        && let Some(action) = step.advance_action
    {
        if *actions >= max {
            return mk(
                phase,
                action,
                "skipped",
                "tick action budget reached".into(),
                mandate_id,
            );
        }
        let mut adv = target.clone();
        adv["action"] = json!(action);
        let adv_ctx = autonomous_prime_ctx(tenant, adv.to_string().into_bytes());
        return match handle_prime_advance(agent_store, spine_store, task_store, &adv_ctx) {
            HandlerOutcome::Ok(b) => {
                let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                if v.get("advanced").and_then(Value::as_bool) == Some(true) {
                    *actions += 1;
                    if let Some(mid) = mandate_id.as_deref() {
                        chronicle_autonomous(
                            task_store,
                            mid,
                            "prime.autonomous_advance",
                            &format!("autonomous Prime advanced `{action}` on mandate {mid}"),
                        );
                    }
                    mk(
                        phase,
                        action,
                        "advanced",
                        format!("ran governed `{action}`"),
                        mandate_id,
                    )
                } else {
                    let refused = v
                        .get("refused")
                        .and_then(Value::as_str)
                        .unwrap_or("not_advanced")
                        .to_string();
                    mk(
                        phase,
                        action,
                        "skipped",
                        format!("advance not applied: {refused}"),
                        mandate_id,
                    )
                }
            }
            // Governance refusal / error — propagate honestly, take no credit.
            HandlerOutcome::Err(e) => mk(
                phase,
                action,
                "blocked",
                format!("advance refused: {}", e.cause),
                mandate_id,
            ),
        };
    }

    // (B) ready_to_start — start ready work for an already-APPROVED Prime
    // proposal through the existing governed `prime.start` path, gated by the
    // autonomous budget hard-stop. A bare Mandate's runs are deliberately left
    // to the heartbeat / `brief.run` (no new start policy invented).
    if step.phase == "ready_to_start" {
        let Some(pid) = step.proposal_id.clone() else {
            return mk(
                phase,
                "none",
                "skipped",
                "bare Mandate ready — runs left to heartbeat/brief.run".into(),
                mandate_id,
            );
        };
        if *actions >= max {
            return mk(
                phase,
                "start",
                "skipped",
                "tick action budget reached".into(),
                mandate_id,
            );
        }
        if let Err(reason) = start_budget_admitted(
            task_store,
            agent_store,
            spine_store,
            metrics,
            tenant,
            &pid,
            now_ms,
        ) {
            return mk(
                phase,
                "start",
                "blocked",
                format!("budget hard-stop: {reason}"),
                mandate_id,
            );
        }
        let start_ctx = autonomous_prime_ctx(tenant, pid.clone().into_bytes());
        return match handle_prime_start(agent_store, spine_store, task_store, registry, &start_ctx)
        {
            HandlerOutcome::Ok(b) => {
                let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                let started = v
                    .get("started")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                if started > 0 {
                    *actions += 1;
                    if let Some(mid) = mandate_id.as_deref() {
                        chronicle_autonomous(
                            task_store,
                            mid,
                            "prime.autonomous_start",
                            &format!(
                                "autonomous Prime started {started} ready Shift(s) for proposal {pid}"
                            ),
                        );
                    }
                    mk(
                        phase,
                        "start",
                        "started",
                        format!("started {started} ready Shift(s)"),
                        mandate_id,
                    )
                } else {
                    mk(
                        phase,
                        "start",
                        "skipped",
                        "no ready Shift actually started".into(),
                        mandate_id,
                    )
                }
            }
            HandlerOutcome::Err(e) => mk(
                phase,
                "start",
                "blocked",
                format!("start refused: {}", e.cause),
                mandate_id,
            ),
        };
    }

    // (B2) needs_hire_approval — STANDING-AUTHORITY governance automation. A
    // pending spawn Clearance / hire is normally a human gate (left `blocked`),
    // but when the Board granted the matching standing authority for THIS Guild
    // the loop may greenlight it on the Board's behalf. Clearances first (mirrors
    // `classify_mandate` priority — greenlighting a Clearance activates its hire),
    // then bare pending hires. Both items are surfaced by `compute_readiness`
    // from the Mandate's own Team Plan, so they are attributable to Prime/company
    // planning by construction; a hire/Clearance outside this Mandate's plan never
    // appears here and is never touched. At most ONE governance action per
    // candidate per tick (the next tick re-classifies and handles the rest).
    if step.phase == "needs_hire_approval" {
        let now_secs = now_secs_from_ms(now_ms);

        // Spawn Clearance — needs `prime.clearance.approve`.
        if let Some(cid) = step
            .pending_clearances
            .first()
            .and_then(|c| c.get("clearance_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if !standing_active(agent_store, tenant, CATEGORY_CLEARANCE_APPROVE, now_secs) {
                return mk(
                    phase,
                    "none",
                    "blocked",
                    "pending spawn Clearance — no prime.clearance.approve standing authority for this Guild".into(),
                    mandate_id,
                );
            }
            if *actions >= max {
                return mk(
                    phase,
                    "clearance_approve",
                    "skipped",
                    "tick action budget reached".into(),
                    mandate_id,
                );
            }
            if !crate::rig::is_known_rig(hire_rig) {
                return mk(
                    phase,
                    "clearance_approve",
                    "skipped",
                    format!(
                        "configured hire rig `{hire_rig}` is not a known Rig — leaving spawn Clearance pending"
                    ),
                    mandate_id,
                );
            }
            return match autonomous_approve_spawn_clearance(
                agent_store,
                tenant,
                cid,
                Some(hire_rig),
            ) {
                Ok(hire_id) => {
                    *actions += 1;
                    let _ =
                        consume_standing(agent_store, tenant, CATEGORY_CLEARANCE_APPROVE, now_secs);
                    if let Some(mid) = mandate_id.as_deref() {
                        chronicle_autonomous(
                            task_store,
                            mid,
                            "prime.autonomous_clearance_approve",
                            &format!(
                                "autonomous Prime greenlit spawn Clearance {cid} (activated hire {hire_id}) on mandate {mid}"
                            ),
                        );
                    }
                    mk(
                        phase,
                        "clearance_approve",
                        "advanced",
                        format!("greenlit spawn Clearance {cid}"),
                        mandate_id,
                    )
                }
                Err(e) => mk(
                    phase,
                    "clearance_approve",
                    "blocked",
                    format!("clearance greenlight refused: {e}"),
                    mandate_id,
                ),
            };
        }

        // Bare pending hire — needs `prime.hire.approve`.
        if let Some(hid) = step
            .pending_hires
            .first()
            .and_then(|h| h.get("agent_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if !standing_active(agent_store, tenant, CATEGORY_HIRE_APPROVE, now_secs) {
                return mk(
                    phase,
                    "none",
                    "blocked",
                    "pending hire — no prime.hire.approve standing authority for this Guild".into(),
                    mandate_id,
                );
            }
            // A misconfigured Rig is SKIPPED (hire left pending), never silently
            // bound — same known-Rig allowlist the manual approve_hire enforces.
            if !crate::rig::is_known_rig(hire_rig) {
                return mk(
                    phase,
                    "hire_approve",
                    "skipped",
                    format!(
                        "configured hire rig `{hire_rig}` is not a known Rig — leaving hire pending"
                    ),
                    mandate_id,
                );
            }
            if *actions >= max {
                return mk(
                    phase,
                    "hire_approve",
                    "skipped",
                    "tick action budget reached".into(),
                    mandate_id,
                );
            }
            return match agent_store.approve_hire_with_rig(hid, Some(hire_rig), tenant) {
                Ok(outcome) => {
                    *actions += 1;
                    let _ = consume_standing(agent_store, tenant, CATEGORY_HIRE_APPROVE, now_secs);
                    let bound = outcome.rig.as_deref().unwrap_or(hire_rig);
                    if let Some(mid) = mandate_id.as_deref() {
                        chronicle_autonomous(
                            task_store,
                            mid,
                            "prime.autonomous_hire_approve",
                            &format!(
                                "autonomous Prime activated hire {hid} on rig {bound} for mandate {mid}"
                            ),
                        );
                    }
                    mk(
                        phase,
                        "hire_approve",
                        "advanced",
                        format!("activated hire {hid} on rig {bound}"),
                        mandate_id,
                    )
                }
                Err(e) => mk(
                    phase,
                    "hire_approve",
                    "blocked",
                    format!("hire activation refused: {e}"),
                    mandate_id,
                ),
            };
        }
        // Fall through (no actionable item) to the human-gate record below.
    }

    // (C) Everything else needs a human gate, or is already running / done —
    // record it, act on nothing, write no event.
    let outcome = match step.phase {
        "needs_approval" | "needs_hire_approval" | "blocked" => "blocked",
        _ => "skipped",
    };
    mk(phase, "none", outcome, step.reason.clone(), mandate_id)
}

/// Run ONE opt-in autonomous Prime tick: discover up to a bounded set of
/// candidates (approved Prime proposals first — they carry Start — then live
/// Mandates not already covered by a proposal) and apply at most `max` safe
/// governed actions across them, returning one [`PrimeAutonomyRecord`] per
/// candidate considered. Pure of any sleeping/timer — the controller calls it on
/// an interval inside `spawn_blocking`.
///
/// Tenant-safe: `tenant=None` spans **all** Guilds (each candidate carries its
/// own `tenant_id` and is processed under it); `tenant=Some(g)` scopes to one
/// Guild. Idempotent: each tick re-classifies live state, so a team plan /
/// orchestration tree / started Shift is never duplicated and an already-
/// running Brief is never double-started. Bounded: `max` caps actions per tick.
#[allow(clippy::too_many_arguments)]
pub fn autonomous_prime_tick(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    metrics: Option<&crate::metrics::MetricsQuery>,
    now_ms: i64,
    max: usize,
    tenant: Option<&str>,
    hire_rig: &str,
) -> Result<Vec<PrimeAutonomyRecord>, String> {
    if max == 0 {
        return Ok(Vec::new());
    }
    let mut records: Vec<PrimeAutonomyRecord> = Vec::new();
    let mut actions = 0usize;
    // Bounded discovery — never an unbounded table scan.
    let discover_cap = max.saturating_mul(4).clamp(max, 50);
    let now_secs = now_secs_from_ms(now_ms);

    // PASS 0 — STANDING-AUTHORITY PROPOSAL APPROVAL. Only for a Guild that
    // granted the `prime.proposal.approve` standing authority: approve PROPOSED
    // Prime proposals through the EXISTING `prime.approve` path (which creates
    // the Mandate + Briefs + crew assignments + pending hires). The proposed list
    // is status-filtered (`status='proposed'`) and tenant-stamped, so a
    // rejected / already-approved / cross-Guild proposal is never approved here,
    // and the standing check is per the proposal's OWN Guild (no cross-tenant
    // grant leak). Idempotent: once approved, the proposal leaves the proposed
    // set, so a re-tick neither re-approves it nor consumes the grant again.
    let proposed = spine_store
        .list_proposed_prime_proposals(tenant, discover_cap)
        .map_err(|e| format!("autonomous prime: list proposed: {e}"))?;
    for p in proposed {
        if actions >= max {
            break;
        }
        // No authority for this proposal's Guild → leave it proposed, silently
        // (no record, so an unauthorized tenant never spams the tick summary).
        if !standing_active(
            agent_store,
            &p.tenant_id,
            CATEGORY_PROPOSAL_APPROVE,
            now_secs,
        ) {
            continue;
        }
        let approve_ctx = autonomous_prime_ctx(&p.tenant_id, p.proposal_id.clone().into_bytes());
        let rec = match handle_prime_approve(agent_store, spine_store, task_store, &approve_ctx) {
            HandlerOutcome::Ok(b) => {
                actions += 1;
                let _ = consume_standing(
                    agent_store,
                    &p.tenant_id,
                    CATEGORY_PROPOSAL_APPROVE,
                    now_secs,
                );
                let v: Value = serde_json::from_slice(&b).unwrap_or(Value::Null);
                let mid = v
                    .get("mandate_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                if let Some(m) = mid.as_deref() {
                    chronicle_autonomous(
                        task_store,
                        m,
                        "prime.autonomous_approve",
                        &format!(
                            "autonomous Prime approved proposal {} (mandate {m})",
                            p.proposal_id
                        ),
                    );
                }
                PrimeAutonomyRecord {
                    tenant: p.tenant_id.clone(),
                    target_kind: "proposal",
                    target_id: p.proposal_id.clone(),
                    mandate_id: mid,
                    phase: "needs_approval".to_string(),
                    action: "approve",
                    outcome: "approved",
                    reason: "materialized proposed plan through the existing prime.approve path"
                        .to_string(),
                }
            }
            HandlerOutcome::Err(e) => PrimeAutonomyRecord {
                tenant: p.tenant_id.clone(),
                target_kind: "proposal",
                target_id: p.proposal_id.clone(),
                mandate_id: None,
                phase: "needs_approval".to_string(),
                action: "approve",
                outcome: "blocked",
                reason: format!("autonomous approve refused: {}", e.cause),
            },
        };
        records.push(rec);
    }

    let proposals = spine_store
        .list_approved_prime_proposals(tenant, discover_cap)
        .map_err(|e| format!("autonomous prime: list proposals: {e}"))?;
    // Mandate ids already covered by a processed proposal — so the bare-Mandate
    // pass does not double-process the same Mandate.
    let mut seen_mandates: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in proposals {
        if actions >= max {
            break;
        }
        if !p.mandate_id.is_empty() {
            seen_mandates.insert(p.mandate_id.clone());
        }
        let target = json!({ "proposal_id": p.proposal_id.clone() });
        records.push(process_candidate(
            agent_store,
            spine_store,
            task_store,
            registry,
            metrics,
            now_ms,
            &p.tenant_id,
            "proposal",
            &p.proposal_id,
            target,
            &mut actions,
            max,
            hire_rig,
        ));
    }

    if actions < max {
        let mandates = spine_store
            .list_active_mandates(tenant, discover_cap)
            .map_err(|e| format!("autonomous prime: list mandates: {e}"))?;
        for m in mandates {
            if actions >= max {
                break;
            }
            if seen_mandates.contains(&m.mandate_id) {
                continue;
            }
            let target = json!({ "mandate_id": m.mandate_id.clone() });
            records.push(process_candidate(
                agent_store,
                spine_store,
                task_store,
                registry,
                metrics,
                now_ms,
                &m.tenant_id,
                "mandate",
                &m.mandate_id,
                target,
                &mut actions,
                max,
                hire_rig,
            ));
        }
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::coordinator::agent::handlers::{fake_ctx_tenant, fake_ctx_with_role};
    use crate::nodes::coordinator::agent::store::AgentStore;
    use crate::nodes::coordinator::spine::store::TeamPlanRecord;

    fn stores() -> (AgentStore, SpineStore, TaskStore) {
        (
            AgentStore::in_memory().unwrap(),
            SpineStore::in_memory().unwrap(),
            TaskStore::in_memory().unwrap(),
        )
    }

    fn ctx(json: Value) -> InvocationCtx {
        fake_ctx_with_role(json.to_string().as_bytes(), "operator", b"caller")
    }

    fn next_step(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &TaskStore,
        target: Value,
    ) -> Value {
        let out = handle_prime_next_step(agents, spine, tasks, &ctx(target));
        match out {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("next_step errored: {}", e.cause),
        }
    }

    fn advance(agents: &AgentStore, spine: &SpineStore, tasks: &TaskStore, target: Value) -> Value {
        let out = handle_prime_advance(agents, spine, tasks, &ctx(target));
        match out {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("advance errored: {}", e.cause),
        }
    }

    fn approved_mandate(spine: &SpineStore) -> String {
        let m = spine
            .create_mandate("default", "Ship v1", "real product", None, None)
            .unwrap();
        spine
            .propose_strategy("default", &m, "build a team")
            .unwrap();
        spine.approve_strategy("default", &m).unwrap();
        m
    }

    /// A genuinely RUNNABLE active Operative of `role` on the safe-local `echo`
    /// Rig (`request_hire` → `approve_hire_with_rig`), so `adopt_active_operative`
    /// (which requires a bound Rig) reuses it.
    fn runnable_operative(agents: &AgentStore, role: &str, seed: &str) -> String {
        let id = agents
            .request_hire(
                "W", role, "W", "eng", "eng", "prime", seed, "medium", "default",
            )
            .unwrap();
        agents
            .approve_hire_with_rig(&id, Some("echo"), "default")
            .unwrap();
        id
    }

    // 1) A proposed (not-yet-approved) proposal → needs approval, no advance.
    #[test]
    fn proposed_proposal_needs_approval_cannot_advance() {
        let (agents, spine, tasks) = stores();
        let pid = spine
            .record_prime_proposal("default", "founder", "ship it", "{}")
            .unwrap();
        let v = next_step(&agents, &spine, &tasks, json!({ "proposal_id": pid }));
        assert_eq!(v["phase"], "needs_approval");
        assert_eq!(v["can_advance"], false);
        assert_eq!(v["action_api"], "prime.approve");
        assert!(v["advance_action"].is_null());

        // Advancing it (with either action) refuses as stale — no side effects.
        let r = advance(
            &agents,
            &spine,
            &tasks,
            json!({ "proposal_id": pid, "action": "create_team_plan" }),
        );
        assert_eq!(r["advanced"], false);
        assert_eq!(r["refused"], "stale_action");
    }

    // 2) An approved Mandate with NO Team Plan → create_team_plan advances, and
    //    the next step then changes (here: to orchestration, having adopted the
    //    active engineer the Guild already has).
    #[test]
    fn approved_mandate_no_plan_advances_create_team_plan() {
        let (agents, spine, tasks) = stores();
        let m = approved_mandate(&spine);
        // A runnable active engineer already on the roster — create_team_plan
        // adopts it (and so the next step becomes orchestration).
        runnable_operative(&agents, "engineer", "subj-e");

        let v = next_step(&agents, &spine, &tasks, json!({ "mandate_id": m }));
        assert_eq!(v["phase"], "needs_team_plan");
        assert_eq!(v["can_advance"], true);
        assert_eq!(v["advance_action"], "create_team_plan");

        let r = advance(
            &agents,
            &spine,
            &tasks,
            json!({ "mandate_id": m, "action": "create_team_plan" }),
        );
        assert_eq!(r["advanced"], true);
        assert_eq!(r["action"], "create_team_plan");
        // A Team Plan now exists.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
        // The next step has changed off needs_team_plan/create_team_plan.
        let after = &r["next_step"];
        assert_eq!(after["phase"], "needs_orchestration");
        assert_eq!(after["advance_action"], "orchestrate_assign_ready");
    }

    // 3) A pending hire → human approval, no advance.
    #[test]
    fn pending_hire_needs_human_approval_cannot_advance() {
        let (agents, spine, tasks) = stores();
        let m = approved_mandate(&spine);
        let pending = agents
            .request_hire(
                "P", "engineer", "P", "eng", "eng", "prime", "subj-p", "medium", "default",
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "x",
                proposed_roles_json: "[]",
                pending_hires_json: &hires,
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "staffing",
            })
            .unwrap();

        let v = next_step(&agents, &spine, &tasks, json!({ "mandate_id": m }));
        assert_eq!(v["phase"], "needs_hire_approval");
        assert_eq!(v["can_advance"], false);
        assert!(v["advance_action"].is_null());
        assert_eq!(v["pending_hires"].as_array().unwrap().len(), 1);

        // Neither advance action is current → both refuse as stale.
        let r = advance(
            &agents,
            &spine,
            &tasks,
            json!({ "mandate_id": m, "action": "orchestrate_assign_ready" }),
        );
        assert_eq!(r["advanced"], false);
        assert_eq!(r["refused"], "stale_action");
    }

    // 4) A ready team → orchestrate_assign_ready advances and creates/assigns
    //    Briefs through the existing orchestration path.
    #[test]
    fn ready_team_advances_orchestrate_assign_ready() {
        let (agents, spine, tasks) = stores();
        let m = approved_mandate(&spine);
        let agent_id = agents
            .create_agent(
                "W", "engineer", "W", "eng", "eng", "prime", "subj-w", "medium", "default",
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{agent_id}\"}}]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "build it",
                proposed_roles_json: "[]",
                pending_hires_json: &hires,
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "staffing",
            })
            .unwrap();

        let v = next_step(&agents, &spine, &tasks, json!({ "mandate_id": m }));
        assert_eq!(v["phase"], "needs_orchestration");
        assert_eq!(v["can_advance"], true);
        assert_eq!(v["advance_action"], "orchestrate_assign_ready");

        let r = advance(
            &agents,
            &spine,
            &tasks,
            json!({ "mandate_id": m, "action": "orchestrate_assign_ready" }),
        );
        assert_eq!(r["advanced"], true);
        assert_eq!(r["result"]["ready"], true);
        assert_eq!(r["result"]["status"], "assigned");
        // Real Briefs were created + assigned under the Mandate.
        let cards = tasks.list_briefs_by_mandate(&m, 50).unwrap();
        assert_eq!(cards.len(), 3, "parent + role track + subject execution");
        assert!(
            !r["result"]["assigned_briefs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    // 5) A stale requested advance_action refuses with NO side effects.
    #[test]
    fn stale_advance_action_refuses_without_side_effects() {
        let (agents, spine, tasks) = stores();
        let m = approved_mandate(&spine);
        // Current step is create_team_plan (no Team Plan yet); request orchestrate
        // instead → stale, with no side effects.
        let r = advance(
            &agents,
            &spine,
            &tasks,
            json!({ "mandate_id": m, "action": "orchestrate_assign_ready" }),
        );
        assert_eq!(r["advanced"], false);
        assert_eq!(r["refused"], "stale_action");
        // No Team Plan and no Briefs were created.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_none());
        assert!(tasks.list_briefs_by_mandate(&m, 50).unwrap().is_empty());
    }

    // 6) Tenant isolation — a Mandate in another Guild reads as not-found, and an
    //    advance against it has no effect.
    #[test]
    fn tenant_isolation_other_guild_not_found() {
        let (agents, spine, tasks) = stores();
        let m = approved_mandate(&spine); // tenant "default"

        // A caller in tenant "other" cannot see it.
        let out = handle_prime_next_step(
            &agents,
            &spine,
            &tasks,
            &fake_ctx_tenant(json!({ "mandate_id": m }).to_string().as_bytes(), "other"),
        );
        match out {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("not found")),
            HandlerOutcome::Ok(_) => panic!("cross-tenant mandate must read as not-found"),
        }

        // And an advance from the other tenant changes nothing in "default".
        agents
            .create_agent(
                "Eng", "engineer", "E", "eng", "eng", "prime", "subj-e", "medium", "default",
            )
            .unwrap();
        let out = handle_prime_advance(
            &agents,
            &spine,
            &tasks,
            &fake_ctx_tenant(
                json!({ "mandate_id": m, "action": "create_team_plan" })
                    .to_string()
                    .as_bytes(),
                "other",
            ),
        );
        assert!(matches!(out, HandlerOutcome::Err(_)));
        assert!(spine.latest_team_plan("default", &m).unwrap().is_none());
    }

    // ── AUTONOMOUS PRIME DRIVER (the opt-in loop) ──────────────────────────

    use crate::nodes::coordinator::agent::handlers::{handle_prime_propose, handle_starter_crew};

    fn echo_registry() -> crate::rig::RigRegistry {
        crate::rig::RigRegistry::with_builtins().with_default("echo")
    }

    /// Run an autonomous Prime tick over one Guild with no metrics (budget gate
    /// inert) and the safe-local `echo` hire Rig — the common shape for the
    /// deterministic team-plan / orchestrate tests below.
    fn tick(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        max: usize,
        tenant: Option<&str>,
    ) -> Vec<PrimeAutonomyRecord> {
        autonomous_prime_tick(agents, spine, tasks, reg, None, 0, max, tenant, "echo").unwrap()
    }

    /// Like [`tick`], but with an explicit hire Rig so the standing-authority
    /// hire tests can exercise the configured-Rig validation path.
    fn tick_rig(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        max: usize,
        tenant: Option<&str>,
        hire_rig: &str,
    ) -> Vec<PrimeAutonomyRecord> {
        autonomous_prime_tick(agents, spine, tasks, reg, None, 0, max, tenant, hire_rig).unwrap()
    }

    /// Grant the synthetic Prime authority a bounded standing approval for
    /// `category` in `tenant` (default `max_calls` unless overridden) — the Board
    /// action the standing-authority driver consumes.
    fn grant_standing(
        agents: &AgentStore,
        tenant: &str,
        category: &str,
        max_calls: Option<i64>,
    ) -> String {
        agents
            .create_scoped_standing(
                crate::nodes::coordinator::agent::store::StandingApprovalCreate {
                    agent_id: AUTONOMOUS_PRIME_AUTHORITY,
                    match_category: category,
                    match_path_glob: None,
                    scope_kind: None,
                    task_id: None,
                    session_id: None,
                    method_prefix: None,
                    workspace_path_glob: None,
                    // Far-future expiry in SECONDS (standing approvals compare `now`
                    // in seconds; the tick passes `now_ms=0` → `now_secs=0`).
                    expires_at: 9_999_999_999,
                    granted_by: "operator",
                    max_calls,
                    max_cost_micros: None,
                    note: "test grant",
                    tenant_id: tenant,
                },
            )
            .unwrap()
    }

    // A) Default-off boundary: the tick is a pure helper — `max == 0` (the
    //    controller passes a clamped 1..=10, but a guard proves no action ever
    //    fires with a zero bound) returns no records / takes no action.
    #[test]
    fn autonomous_tick_with_zero_bound_does_nothing() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");
        let recs = tick(&agents, &spine, &tasks, &reg, 0, Some("default"));
        assert!(recs.is_empty());
        // No Team Plan was recorded.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_none());
    }

    // B) An approved Mandate at `needs_team_plan` is advanced by the loop through
    //    the SAME governed team-plan route (adopts the active crew, mints no
    //    hires).
    #[test]
    fn autonomous_tick_advances_needs_team_plan() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_team_plan");
        assert_eq!(rec.action, "create_team_plan");
        assert_eq!(rec.outcome, "advanced");
        // A Team Plan now exists, recorded through the governed route.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // C) A ready team at `needs_orchestration` is advanced by the loop through the
    //    existing orchestration gate (creates + assigns the Brief tree).
    #[test]
    fn autonomous_tick_advances_needs_orchestration() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        let agent_id = agents
            .create_agent(
                "W", "engineer", "W", "eng", "eng", "prime", "subj-w", "medium", "default",
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{agent_id}\"}}]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "build it",
                proposed_roles_json: "[]",
                pending_hires_json: &hires,
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "staffing",
            })
            .unwrap();

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_orchestration");
        assert_eq!(rec.action, "orchestrate_assign_ready");
        assert_eq!(rec.outcome, "advanced");
        // The real Brief tree was created + assigned under the Mandate.
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
    }

    // D) Idempotency: re-ticking after the orchestration tree exists never
    //    creates a SECOND tree — `mandate.orchestrate` reuses Briefs by source
    //    marker, so the Brief count is stable across repeated ticks (the loop may
    //    re-run the idempotent orchestrate to assign a still-unassigned track,
    //    but it duplicates nothing).
    #[test]
    fn autonomous_tick_orchestration_is_idempotent() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        let agent_id = agents
            .create_agent(
                "W", "engineer", "W", "eng", "eng", "prime", "subj-w", "medium", "default",
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{agent_id}\"}}]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "build it",
                proposed_roles_json: "[]",
                pending_hires_json: &hires,
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "staffing",
            })
            .unwrap();

        let _ = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let after_first = tasks.list_briefs_by_mandate(&m, 50).unwrap().len();
        assert_eq!(after_first, 3);
        // Two more ticks must not create a single extra Brief (no duplicate tree).
        let _ = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let _ = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        assert_eq!(
            tasks.list_briefs_by_mandate(&m, 50).unwrap().len(),
            after_first,
            "repeated ticks must not duplicate the orchestration tree"
        );
    }

    // E) Governance: the loop NEVER auto-approves a pending hire — it records a
    //    blocked result and leaves the hire `pending`.
    #[test]
    fn autonomous_tick_does_not_auto_approve_pending_hire() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        let pending = agents
            .request_hire(
                "P", "engineer", "P", "eng", "eng", "prime", "subj-p", "medium", "default",
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "x",
                proposed_roles_json: "[]",
                pending_hires_json: &hires,
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "staffing",
            })
            .unwrap();

        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_hire_approval");
        assert_eq!(rec.action, "none");
        assert_eq!(rec.outcome, "blocked");
        // The hire is still pending — the loop greenlit nothing, created no Briefs.
        assert_eq!(
            agents.get_agent(&pending).unwrap().unwrap().status,
            "pending"
        );
        assert!(tasks.list_briefs_by_mandate(&m, 50).unwrap().is_empty());
    }

    // F) Tenant isolation: a tick for Guild "other" never acts on a "default"
    //    Mandate.
    #[test]
    fn autonomous_tick_is_tenant_isolated() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine); // tenant "default"
        runnable_operative(&agents, "engineer", "subj-e");

        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("other"));
        assert!(
            recs.iter().all(|r| r.target_id != m),
            "a tick for `other` must not consider a `default` Mandate"
        );
        // No Team Plan was created for the default Mandate.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_none());
    }

    // G) End-to-end Start: an approved Prime PROPOSAL that reaches ready_to_start
    //    is started by the loop through the existing governed `prime.start` path,
    //    and a second tick does not double-start the now-running/started work.
    #[tokio::test]
    async fn autonomous_tick_starts_ready_approved_proposal() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        // Empty company → starter crew (Founder + safe-local echo workers).
        let _ = handle_starter_crew(&agents, &fake_ctx_with_role(b"", "operator", b"caller"));
        // Propose → approve creates the Mandate + Briefs + crew assignments.
        let propose_ctx = fake_ctx_with_role(b"Build a sales dashboard", "operator", b"caller");
        let propose = match handle_prime_propose(&agents, &spine, &propose_ctx) {
            HandlerOutcome::Ok(b) => {
                let v: Value = serde_json::from_slice(&b).unwrap();
                v["proposal_id"].as_str().unwrap().to_string()
            }
            HandlerOutcome::Err(e) => panic!("propose: {}", e.cause),
        };
        let approve_ctx = fake_ctx_with_role(propose.as_bytes(), "operator", b"caller");
        match handle_prime_approve(&agents, &spine, &tasks, &approve_ctx) {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("approve: {}", e.cause),
        }

        // The loop discovers the approved proposal and starts its ready Briefs.
        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_kind == "proposal" && r.outcome == "started")
            .expect("an approved proposal's ready work is started by the loop");
        assert_eq!(rec.phase, "ready_to_start");
        assert_eq!(rec.action, "start");
        let runs_after_first = tasks.list_runs_for_tenant("default", 100).unwrap().len();
        assert!(runs_after_first > 0, "at least one Shift run was opened");

        // Idempotency: a second immediate tick does not re-start the already
        // claimed/running Briefs (no new started records, no extra runs).
        let recs2 = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        assert!(
            recs2.iter().all(|r| r.outcome != "started"),
            "a running proposal must not be double-started"
        );
    }

    // ── PRIME STANDING AUTHORITY (v1) ──────────────────────────────────────

    /// Propose a deterministic plan and return its (proposed) proposal id.
    fn propose_pid(agents: &AgentStore, spine: &SpineStore) -> String {
        let _ = handle_starter_crew(agents, &fake_ctx_with_role(b"", "operator", b"caller"));
        let ctx = fake_ctx_with_role(b"Build a sales dashboard", "operator", b"caller");
        match handle_prime_propose(agents, spine, &ctx) {
            HandlerOutcome::Ok(b) => serde_json::from_slice::<Value>(&b).unwrap()["proposal_id"]
                .as_str()
                .unwrap()
                .to_string(),
            HandlerOutcome::Err(e) => panic!("propose: {}", e.cause),
        }
    }

    // H) No standing authority → a proposed proposal is left proposed (the loop
    //    never approves a Prime proposal from env alone).
    #[test]
    fn autonomous_tick_without_standing_leaves_proposal_proposed() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let pid = propose_pid(&agents, &spine);

        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        assert!(
            recs.iter().all(|r| r.outcome != "approved"),
            "no standing authority ⇒ no autonomous approval"
        );
        assert_eq!(
            spine
                .get_prime_proposal("default", &pid)
                .unwrap()
                .unwrap()
                .status,
            "proposed",
            "the proposal must remain proposed"
        );
    }

    // I) With `prime.proposal.approve` standing → the proposed proposal is
    //    approved through the existing prime.approve path, the max bound is
    //    honored, and a bounded grant's single call is consumed.
    #[test]
    fn autonomous_tick_with_standing_approves_proposal_bounded() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let pid = propose_pid(&agents, &spine);
        // Bounded to exactly one approval.
        grant_standing(&agents, "default", CATEGORY_PROPOSAL_APPROVE, Some(1));

        // max=1 ⇒ only the approval action fires this tick.
        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_kind == "proposal" && r.outcome == "approved")
            .expect("the proposed proposal is approved by the loop");
        assert_eq!(rec.action, "approve");
        assert_eq!(rec.phase, "needs_approval");

        let row = spine.get_prime_proposal("default", &pid).unwrap().unwrap();
        assert_eq!(row.status, "approved");
        assert!(
            !row.mandate_id.is_empty(),
            "approval materialized a Mandate"
        );
        // Real Briefs were created through the governed approve path.
        assert!(
            !tasks
                .list_briefs_by_mandate(&row.mandate_id, 50)
                .unwrap()
                .is_empty()
        );

        // The bounded (max_calls=1) grant is now exhausted.
        assert!(
            !agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_PROPOSAL_APPROVE, 1)
                .unwrap(),
            "a bounded standing grant is consumed when the approval is taken"
        );
    }

    // J) Re-tick idempotency: an already-approved proposal is not re-approved and
    //    the standing grant is not consumed a second time. (`tokio::test` because
    //    a larger budget lets the approved proposal proceed to Start, which funnels
    //    through the run preflight's reactor.)
    #[tokio::test]
    async fn autonomous_tick_proposal_approval_is_idempotent() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let pid = propose_pid(&agents, &spine);
        // Allow two calls so a (wrong) double-consume would be observable.
        grant_standing(&agents, "default", CATEGORY_PROPOSAL_APPROVE, Some(2));

        let _ = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        let row1 = spine.get_prime_proposal("default", &pid).unwrap().unwrap();
        assert_eq!(row1.status, "approved");
        let mandate1 = row1.mandate_id.clone();
        let briefs1 = tasks.list_briefs_by_mandate(&mandate1, 50).unwrap().len();
        let used1 = agents
            .list_standing_for_tenant(AUTONOMOUS_PRIME_AUTHORITY, "default")
            .unwrap()[0]
            .calls_used;
        assert_eq!(used1, 1, "exactly one approval call consumed");

        // Re-tick: the proposal is no longer proposed, so nothing re-approves it.
        let recs2 = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        assert!(
            recs2
                .iter()
                .all(|r| !(r.target_id == pid && r.outcome == "approved")),
            "an already-approved proposal must not be re-approved"
        );
        let row2 = spine.get_prime_proposal("default", &pid).unwrap().unwrap();
        assert_eq!(row2.mandate_id, mandate1, "no second Mandate");
        assert_eq!(
            tasks.list_briefs_by_mandate(&mandate1, 50).unwrap().len(),
            briefs1,
            "no duplicate Briefs"
        );
        let used2 = agents
            .list_standing_for_tenant(AUTONOMOUS_PRIME_AUTHORITY, "default")
            .unwrap()[0]
            .calls_used;
        assert_eq!(used2, 1, "the grant is not consumed again on a re-tick");
    }

    // K) Tenant isolation: a standing grant in Guild A never approves Guild B's
    //    proposal. A cross-Guild tick (tenant=None) approves only the granted
    //    Guild's proposal. (`tokio::test` — the granted Guild's approved work may
    //    proceed to Start through the run preflight's reactor.)
    #[tokio::test]
    async fn standing_authority_is_tenant_isolated() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        // A proposed proposal in each Guild.
        let pid_default = propose_pid(&agents, &spine);
        let other_ctx = fake_ctx_tenant(b"Build a sales dashboard", "other");
        let pid_other = match handle_prime_propose(&agents, &spine, &other_ctx) {
            HandlerOutcome::Ok(b) => serde_json::from_slice::<Value>(&b).unwrap()["proposal_id"]
                .as_str()
                .unwrap()
                .to_string(),
            HandlerOutcome::Err(e) => panic!("propose other: {}", e.cause),
        };
        // Grant ONLY in "default".
        grant_standing(&agents, "default", CATEGORY_PROPOSAL_APPROVE, None);

        // Drive ALL Guilds.
        let _ = tick(&agents, &spine, &tasks, &reg, 10, None);

        assert_eq!(
            spine
                .get_prime_proposal("default", &pid_default)
                .unwrap()
                .unwrap()
                .status,
            "approved",
            "the granted Guild's proposal is approved"
        );
        assert_eq!(
            spine
                .get_prime_proposal("other", &pid_other)
                .unwrap()
                .unwrap()
                .status,
            "proposed",
            "a grant in `default` must never approve `other`'s proposal"
        );
    }

    /// An approved Mandate carrying a single PENDING hire in its Team Plan — the
    /// `needs_hire_approval` shape the hire-approve standing authority acts on.
    fn mandate_with_pending_hire(agents: &AgentStore, spine: &SpineStore) -> (String, String) {
        let m = approved_mandate(spine);
        let pending = agents
            .request_hire(
                "P", "engineer", "P", "eng", "eng", "prime", "subj-p", "medium", "default",
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "x",
                proposed_roles_json: "[]",
                pending_hires_json: &hires,
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "staffing",
            })
            .unwrap();
        (m, pending)
    }

    // L) With `prime.hire.approve` standing → an attributable pending hire is
    //    activated and bound to the configured safe Rig; without it, the hire
    //    stays pending.
    #[test]
    fn standing_hire_approve_activates_pending_hire_on_configured_rig() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, pending) = mandate_with_pending_hire(&agents, &spine);
        grant_standing(&agents, "default", CATEGORY_HIRE_APPROVE, Some(1));

        let recs = tick_rig(&agents, &spine, &tasks, &reg, 1, Some("default"), "echo");
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_hire_approval");
        assert_eq!(rec.action, "hire_approve");
        assert_eq!(rec.outcome, "advanced");

        let agent = agents.get_agent(&pending).unwrap().unwrap();
        assert_eq!(agent.status, "active", "the hire is activated");
        assert_eq!(
            agent.rig.as_deref(),
            Some("echo"),
            "bound to the configured Rig"
        );
        assert!(
            !agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_HIRE_APPROVE, 1)
                .unwrap(),
            "the bounded hire grant is consumed"
        );
    }

    // M) An unknown configured hire Rig is REFUSED/SKIPPED — never silently
    //    bound — and the hire is left pending (no consume).
    #[test]
    fn standing_hire_approve_refuses_unknown_rig() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, pending) = mandate_with_pending_hire(&agents, &spine);
        grant_standing(&agents, "default", CATEGORY_HIRE_APPROVE, Some(1));

        let recs = tick_rig(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            "bogus-rig",
        );
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "hire_approve");
        assert_eq!(
            rec.outcome, "skipped",
            "an unknown Rig is skipped, not bound"
        );

        let agent = agents.get_agent(&pending).unwrap().unwrap();
        assert_eq!(
            agent.status, "pending",
            "the hire stays pending on a bad Rig"
        );
        assert!(agent.rig.is_none(), "no bad Rig was bound");
        assert!(
            agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_HIRE_APPROVE, 1)
                .unwrap(),
            "a skipped action does not consume the grant"
        );
    }

    // N) Without `prime.hire.approve` standing, even with the driver running, a
    //    pending hire is left untouched (blocked, not activated).
    #[test]
    fn standing_hire_approve_requires_grant() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, pending) = mandate_with_pending_hire(&agents, &spine);
        // No grant.
        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.outcome, "blocked");
        assert_eq!(
            agents.get_agent(&pending).unwrap().unwrap().status,
            "pending"
        );
    }

    // O) With `prime.clearance.approve` standing → an attributable pending spawn
    //    Clearance is greenlit (activating its hire); an unrelated NON-spawn
    //    approval is never touched.
    #[test]
    fn standing_clearance_approve_greenlights_attributable_clearance_only() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        let pending = agents
            .request_hire(
                "P", "engineer", "P", "eng", "eng", "prime", "subj-cl", "medium", "default",
            )
            .unwrap();
        let cid = agents
            .create_spawn_clearance(&pending, "subj-cl", "spawn the hire", &[], "default")
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]");
        let clearances = format!("[\"{cid}\"]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "x",
                proposed_roles_json: "[]",
                pending_hires_json: &hires,
                clearance_ids_json: &clearances,
                denials_json: "[]",
                next_steps_json: "[]",
                status: "awaiting_clearance",
            })
            .unwrap();
        // An UNRELATED non-spawn approval that must remain pending.
        let arbitrary = agents
            .create_approval(
                "subj-x",
                "subj-x",
                "tool.shell",
                "tool",
                "hash",
                "run a tool",
                &[],
                None,
                9_999_999_999,
                &[],
                "default",
            )
            .unwrap();
        grant_standing(&agents, "default", CATEGORY_CLEARANCE_APPROVE, Some(1));

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_hire_approval");
        assert_eq!(rec.action, "clearance_approve");
        assert_eq!(rec.outcome, "advanced");

        // The spawn Clearance is approved and the hire is now active+runnable.
        assert_eq!(
            agents
                .get_approval_record_for_tenant(&cid, "default")
                .unwrap()
                .unwrap()
                .status
                .as_wire(),
            "approved"
        );
        let activated = agents.get_agent(&pending).unwrap().unwrap();
        assert_eq!(activated.status, "active");
        assert_eq!(
            activated.rig.as_deref(),
            Some("echo"),
            "autonomous Clearance approval binds the configured Rig"
        );
        // The unrelated tool approval is untouched.
        assert_eq!(
            agents
                .get_approval_record_for_tenant(&arbitrary, "default")
                .unwrap()
                .unwrap()
                .status
                .as_wire(),
            "pending",
            "an arbitrary non-spawn approval is never auto-approved"
        );
    }

    #[test]
    fn standing_clearance_approve_refuses_unknown_rig() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        let pending = agents
            .request_hire(
                "P",
                "engineer",
                "P",
                "eng",
                "eng",
                "prime",
                "subj-cl-bad",
                "medium",
                "default",
            )
            .unwrap();
        let cid = agents
            .create_spawn_clearance(&pending, "subj-cl-bad", "spawn the hire", &[], "default")
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]");
        let clearances = format!("[\"{cid}\"]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "x",
                proposed_roles_json: "[]",
                pending_hires_json: &hires,
                clearance_ids_json: &clearances,
                denials_json: "[]",
                next_steps_json: "[]",
                status: "awaiting_clearance",
            })
            .unwrap();
        grant_standing(&agents, "default", CATEGORY_CLEARANCE_APPROVE, Some(1));

        let recs = tick_rig(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            "bogus-rig",
        );
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "clearance_approve");
        assert_eq!(rec.outcome, "skipped");

        assert_eq!(
            agents
                .get_approval_record_for_tenant(&cid, "default")
                .unwrap()
                .unwrap()
                .status
                .as_wire(),
            "pending",
            "bad Rig config leaves the Clearance pending"
        );
        let agent = agents.get_agent(&pending).unwrap().unwrap();
        assert_eq!(agent.status, "pending");
        assert!(agent.rig.is_none());
        assert!(
            agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_CLEARANCE_APPROVE, 1)
                .unwrap(),
            "a skipped Clearance action does not consume the grant"
        );
    }

    // P) The read-only `prime.standing_authority` surface reflects live grant
    //    state for the caller's Guild.
    #[test]
    fn standing_authority_surface_reports_live_state() {
        let (agents, _spine, _tasks) = stores();
        grant_standing(&agents, "default", CATEGORY_HIRE_APPROVE, None);
        let out = handle_prime_standing_authority(
            &agents,
            &fake_ctx_with_role(b"", "operator", b"caller"),
        );
        let v: Value = match out {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("standing_authority errored: {}", e.cause),
        };
        assert_eq!(v["authority_id"], AUTONOMOUS_PRIME_AUTHORITY);
        let cats = v["categories"].as_array().unwrap();
        let active_of = |cat: &str| -> bool {
            cats.iter()
                .find(|c| c["category"] == cat)
                .map(|c| c["active"].as_bool().unwrap())
                .unwrap()
        };
        assert!(
            active_of(CATEGORY_HIRE_APPROVE),
            "the granted category is active"
        );
        assert!(
            !active_of(CATEGORY_PROPOSAL_APPROVE),
            "an ungranted category is inactive"
        );
        assert!(!active_of(CATEGORY_CLEARANCE_APPROVE));
    }

    // Q) The operator grant/revoke control path (the Settings "Grant"/"Revoke"
    //    buttons): a standing grant CREATED through the EXISTING
    //    `agent.standing_approval.create` handler for the synthetic Prime
    //    authority flips the read-only `prime.standing_authority` surface to
    //    active, is LISTABLE by category through `agent.standing_approval.list`,
    //    and REVOKING that row through `agent.standing_approval.revoke` flips the
    //    surface back to inactive — proving the dashboard reuses the same
    //    handler/store path real Operatives use, with no bespoke route.
    #[test]
    fn standing_authority_grant_list_revoke_through_handlers() {
        use crate::nodes::coordinator::agent::handlers::{
            handle_standing_create, handle_standing_list, handle_standing_revoke,
        };
        let (agents, _spine, _tasks) = stores();

        let active_of = |cat: &str| -> bool {
            let out = handle_prime_standing_authority(
                &agents,
                &fake_ctx_with_role(b"", "operator", b"caller"),
            );
            let v: Value = match out {
                HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
                HandlerOutcome::Err(e) => panic!("standing_authority errored: {}", e.cause),
            };
            v["categories"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["category"] == cat)
                .map(|c| c["active"].as_bool().unwrap_or(false))
                .unwrap_or(false)
        };

        // Initially ungranted ⇒ inactive.
        assert!(!active_of(CATEGORY_PROPOSAL_APPROVE));

        // GRANT through the create handler using the bridge's POST forward shape
        // (the synthetic authority id + the bounded defaults the Settings panel
        // sends: a far-future expiry in seconds, a 25-call cap, no cost cap).
        let grant = json!({
            "agent_id": AUTONOMOUS_PRIME_AUTHORITY,
            "category": CATEGORY_PROPOSAL_APPROVE,
            "expires_at": 9_999_999_999i64,
            "granted_by": "operator",
            "max_calls": 25,
            "note": "from settings",
        });
        let standing_id = match handle_standing_create(
            &agents,
            &fake_ctx_with_role(grant.to_string().as_bytes(), "operator", b"caller"),
        ) {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap().trim().to_string(),
            HandlerOutcome::Err(e) => panic!("standing create errored: {}", e.cause),
        };
        assert!(!standing_id.is_empty());
        assert!(
            active_of(CATEGORY_PROPOSAL_APPROVE),
            "a grant flips the read surface active"
        );

        // LIST through the list handler returns the row for the synthetic
        // authority (the id the dashboard revoke resolves from the category).
        let list = match handle_standing_list(
            &agents,
            &fake_ctx_with_role(AUTONOMOUS_PRIME_AUTHORITY.as_bytes(), "operator", b"caller"),
        ) {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("standing list errored: {}", e.cause),
        };
        assert!(
            list.contains(&standing_id),
            "listing surfaces the granted row"
        );
        assert!(list.contains(CATEGORY_PROPOSAL_APPROVE));

        // REVOKE that row through the revoke handler ⇒ surface back to inactive.
        match handle_standing_revoke(
            &agents,
            &fake_ctx_with_role(standing_id.as_bytes(), "operator", b"caller"),
        ) {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("standing revoke errored: {}", e.cause),
        };
        assert!(
            !active_of(CATEGORY_PROPOSAL_APPROVE),
            "revoking the row flips the read surface inactive"
        );
    }

    // ── PRIME RUNTIME AUTONOMY SWITCH (v1) ──────────────────────────────────

    // Q) The pure effective-state resolver: env wins, else runtime, else off.
    #[test]
    fn effective_autonomy_resolves_source() {
        assert_eq!(effective_autonomy(false, false), (false, "off"));
        assert_eq!(effective_autonomy(false, true), (true, "runtime"));
        assert_eq!(effective_autonomy(true, false), (true, "env"));
        // Env override wins even if runtime is also on.
        assert_eq!(effective_autonomy(true, true), (true, "env"));
    }

    // R) The pure drive planner: env → all guilds; else only the runtime-on
    //    tenants; an empty enabled set is dormant. A runtime-off tenant is
    //    NEVER driven unless env override is on.
    #[test]
    fn plan_autonomy_drive_decides_what_to_run() {
        assert_eq!(plan_autonomy_drive(false, vec![]), AutonomyDrive::Dormant);
        assert_eq!(
            plan_autonomy_drive(false, vec!["acme".into(), "globex".into()]),
            AutonomyDrive::Tenants(vec!["acme".into(), "globex".into()])
        );
        // Env override drives ALL guilds regardless of the runtime list.
        assert_eq!(plan_autonomy_drive(true, vec![]), AutonomyDrive::AllGuilds);
        assert_eq!(
            plan_autonomy_drive(true, vec!["acme".into()]),
            AutonomyDrive::AllGuilds
        );
    }

    // S) The read capability defaults OFF and reflects a persisted ON, and the
    //    setter persists + is tenant-scoped. (Env is unset in the test env, so
    //    effective == runtime here.)
    #[test]
    fn autonomy_state_read_and_set_roundtrip() {
        let (_, spine, _) = stores();

        // Default: nothing persisted → off / source off.
        let v = match handle_prime_autonomy_state(&spine, &fake_ctx_tenant(b"", "acme")) {
            HandlerOutcome::Ok(b) => serde_json::from_slice::<Value>(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("state errored: {}", e.cause),
        };
        assert_eq!(v["runtime_enabled"], false);
        assert_eq!(v["effective_enabled"], false);
        assert_eq!(v["source"], "off");

        // Turn it ON for acme.
        let set = json!({ "enabled": true }).to_string();
        let v = match handle_prime_autonomy_set(&spine, &fake_ctx_tenant(set.as_bytes(), "acme")) {
            HandlerOutcome::Ok(b) => serde_json::from_slice::<Value>(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("set errored: {}", e.cause),
        };
        assert_eq!(v["runtime_enabled"], true);
        assert_eq!(v["effective_enabled"], true);
        assert_eq!(v["source"], "runtime");

        // A fresh read of acme reflects ON; another Guild stays OFF (isolation).
        let acme = match handle_prime_autonomy_state(&spine, &fake_ctx_tenant(b"", "acme")) {
            HandlerOutcome::Ok(b) => serde_json::from_slice::<Value>(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("state errored: {}", e.cause),
        };
        assert_eq!(acme["runtime_enabled"], true);
        let globex = match handle_prime_autonomy_state(&spine, &fake_ctx_tenant(b"", "globex")) {
            HandlerOutcome::Ok(b) => serde_json::from_slice::<Value>(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("state errored: {}", e.cause),
        };
        assert_eq!(globex["runtime_enabled"], false);

        // Turn it back OFF.
        let off = json!({ "enabled": false }).to_string();
        let v = match handle_prime_autonomy_set(&spine, &fake_ctx_tenant(off.as_bytes(), "acme")) {
            HandlerOutcome::Ok(b) => serde_json::from_slice::<Value>(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("set errored: {}", e.cause),
        };
        assert_eq!(v["runtime_enabled"], false);
        assert_eq!(v["source"], "off");
    }

    // T) The setter is role-gated: a worker subject cannot flip it.
    #[test]
    fn autonomy_set_is_operator_only() {
        let (_, spine, _) = stores();
        let set = json!({ "enabled": true }).to_string();
        let out = handle_prime_autonomy_set(
            &spine,
            &fake_ctx_with_role(set.as_bytes(), "agent", b"worker"),
        );
        match out {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, relix_core::types::error_kinds::POLICY_DENIED)
            }
            HandlerOutcome::Ok(_) => panic!("a worker must not toggle autonomy"),
        }
        // And nothing was persisted.
        assert_eq!(
            spine
                .get_runtime_setting_bool("default", RUNTIME_KEY_AUTONOMOUS_PRIME)
                .unwrap(),
            None
        );
    }

    // U) A malformed body is a clean invalid-args refusal (→ 400 at the bridge),
    //    not a panic or a silent default.
    #[test]
    fn autonomy_set_rejects_malformed_body() {
        let (_, spine, _) = stores();
        let out =
            handle_prime_autonomy_set(&spine, &fake_ctx_with_role(b"not json", "operator", b"c"));
        match out {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, relix_core::types::error_kinds::INVALID_ARGS)
            }
            HandlerOutcome::Ok(_) => panic!("malformed body must be refused"),
        }
    }
}
