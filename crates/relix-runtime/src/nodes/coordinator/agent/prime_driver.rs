//! Prime guided driver (v1) — `company-model.md §5.4/§8.2` (the Action Center /
//! Board "next governed step", computed from live state and routed to existing
//! gates) focused onto a SINGLE Prime work session, plus `§12.5/§12.5B` (the
//! Prime planner + `prime.start`).
//!
//! This is a **bounded guide**, NOT an autonomous CEO. Two capabilities:
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

use serde::Deserialize;
use serde_json::{Value, json};

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::coordinator::TaskStore;
use crate::nodes::coordinator::agent::handlers::{
    ReadinessView, brief_status_row, compute_readiness, handle_orchestrate, handle_team_plan,
    internal, invalid,
};
use crate::nodes::coordinator::agent::prime;
use crate::nodes::coordinator::agent::store::AgentStore;
use crate::nodes::coordinator::spine::SpineStore;

/// The one-step advance keys the driver may execute on explicit operator
/// request. Strategy / hire / spawn / budget approvals are deliberately NOT
/// here — they stay human decisions.
const ADVANCE_CREATE_TEAM_PLAN: &str = "create_team_plan";
const ADVANCE_ORCHESTRATE: &str = "orchestrate_assign_ready";

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
    for p in agent_store.list_active_for_tenant(tenant).unwrap_or_default() {
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

    if let Some(pid) = args.proposal_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
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

    if let Some(mid) = args.mandate_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
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
        return classify_mandate(agent_store, spine_store, task_store, tenant, None, mid, brief_ids);
    }

    Err(invalid("prime.next_step: proposal_id or mandate_id required".into()))
}

/// The next step for a proposal that has not been approved yet.
fn proposal_pre_approval_step(row: &crate::nodes::coordinator::spine::store::PrimeProposalRow) -> NextStep {
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
    let strategy = spine_store.strategy_status(tenant, mandate_id).unwrap_or(None);
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
                "The Mandate strategy was rejected. Propose a new strategy to continue."
                    .into(),
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
                     explicit Start control — the driver does not auto-run real adapters.",
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
                format!("{} Shift(s) are running — inspect progress.", counts.running),
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

    fn advance(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &TaskStore,
        target: Value,
    ) -> Value {
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
        spine.propose_strategy("default", &m, "build a team").unwrap();
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
        assert!(!r["result"]["assigned_briefs"].as_array().unwrap().is_empty());
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
}
