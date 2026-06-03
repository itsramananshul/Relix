//! Capability handlers for the agent permission model.
//!
//! Wire formats land alongside each handler in the body
//! comment; the top-level table is documented in
//! `docs/agent-permissions.md`. Handlers live in a separate
//! file so the store module stays focused on storage.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::coordinator::agent::keys::{
    KeyVerdict, assign_verdict, configure_verdict, manage_verdict, spawn_verdict,
};
use crate::nodes::coordinator::agent::store::{
    AgentStore, AgentStoreError, ApprovalStatus, StandingApprovalCreate,
    default_approval_categories,
};
use crate::nodes::coordinator::spine::SpineStore;
use crate::nodes::coordinator::spine::store::{SpineStoreError, TeamPlanRecord};
use crate::nodes::coordinator::{CoordinatorError, TaskStore};

// ── agent.create ─────────────────────────────────────────

/// Wire arg: `name|role|title|department|team|created_by|subject_id|risk_ceiling`
pub fn handle_create(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    // `agent.create` mints an **active** Operative directly — the
    // Founder/Board escape hatch. An *agent* actor must not use it to
    // conjure a live colleague (company-model §4.4 / §5.2A): it is
    // routed to `agent.request_hire`, which mints a pending-inert hire
    // and is gated by the spawn Key.
    if !caller_is_operator(ctx) {
        return policy_denied(
            "agent.create is operator-only; an Operative must use agent.request_hire \
             (spawn Key + pending approval)"
                .to_string(),
        );
    }
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.create utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(8, '|').collect();
    if parts.len() != 8 {
        return invalid(
            "agent.create: expected `name|role|title|department|team|created_by|subject_id|risk_ceiling`".into(),
        );
    }
    match store.create_agent(
        parts[0],
        parts[1],
        parts[2],
        parts[3],
        parts[4],
        parts[5],
        parts[6],
        parts[7],
        ctx.tenant_id_or_default(),
    ) {
        Ok(id) => HandlerOutcome::Ok(format!("{id}\n").into_bytes()),
        Err(AgentStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("agent.create: {e}")),
    }
}

/// `agent.request_hire` — the **gated** creation path (company-model
/// §4.4 / §5.5): mints the Operative `pending` (inert — the gate
/// denies non-active) so a Lead/Founder must approve it before it can
/// run, be assigned, or hold Keys. Same arg shape as `agent.create`.
/// Returns the new agent_id.
pub fn handle_request_hire(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.request_hire utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(8, '|').collect();
    if parts.len() != 8 {
        return invalid(
            "agent.request_hire: expected `name|role|title|department|team|created_by|subject_id|risk_ceiling`".into(),
        );
    }
    // Spawn-Key gate (company-model §5.2A): an agent actor needs
    // `can_spawn_agents`. The Founder/Board bypasses; a denied actor
    // never mints a hire. On lead/founder route a typed spawn
    // Clearance is created that must be greenlit to activate the hire.
    let gate = match enforce_spawn_key(store, ctx) {
        Ok(g) => g,
        Err(out) => return out,
    };
    match store.request_hire(
        parts[0],
        parts[1],
        parts[2],
        parts[3],
        parts[4],
        parts[5],
        parts[6],
        parts[7],
        ctx.tenant_id_or_default(),
    ) {
        // parts[6] is the hire's subject_id.
        Ok(id) => finalize_spawn(store, ctx, &id, parts[6], gate),
        Err(AgentStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("agent.request_hire: {e}")),
    }
}

/// `agent.request_hire_for_mandate` — the strategy-gated team-build
/// path. Arg:
/// `mandate_id|name|role|title|department|team|created_by|subject_id|risk_ceiling`.
///
/// This is deliberately separate from `agent.request_hire` so the
/// legacy/manual hire flow stays stable while the Prime/CEO flow gets
/// a hard, queryable strategy precondition.
pub fn handle_request_hire_for_mandate(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.request_hire_for_mandate utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(9, '|').collect();
    if parts.len() != 9 {
        return invalid(
            "agent.request_hire_for_mandate: expected `mandate_id|name|role|title|department|team|created_by|subject_id|risk_ceiling`"
                .into(),
        );
    }
    let mandate_id = parts[0].trim();
    if mandate_id.is_empty() {
        return invalid("agent.request_hire_for_mandate: mandate_id required".into());
    }
    match spine_store.strategy_approved(ctx.tenant_id_or_default(), mandate_id) {
        Ok(true) => {}
        Ok(false) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::POLICY_DENIED,
                cause: format!(
                    "agent.request_hire_for_mandate: mandate `{mandate_id}` strategy is not approved"
                ),
                retry_hint: 0,
                retry_after: None,
            });
        }
        Err(SpineStoreError::BadInput(m)) | Err(SpineStoreError::NotFound(m)) => {
            return invalid(format!("agent.request_hire_for_mandate: {m}"));
        }
        Err(e) => return internal(format!("agent.request_hire_for_mandate: {e}")),
    }
    // Strategy is approved; now the actor still needs the spawn Key
    // (company-model §5.2A) — the two gates are independent. The
    // strategy gate above always runs first, so a spawn Clearance is
    // only ever minted for a strategy-approved Mandate.
    let gate = match enforce_spawn_key(agent_store, ctx) {
        Ok(g) => g,
        Err(out) => return out,
    };
    match agent_store.request_hire(
        parts[1],
        parts[2],
        parts[3],
        parts[4],
        parts[5],
        parts[6],
        parts[7],
        parts[8],
        ctx.tenant_id_or_default(),
    ) {
        // parts[7] is the hire's subject_id (mandate_id is parts[0]).
        Ok(id) => finalize_spawn(agent_store, ctx, &id, parts[7], gate),
        Err(AgentStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("agent.request_hire_for_mandate: {e}")),
    }
}

/// `mandate.team_plan` — the **Prime team-build foundation**
/// (company-model §4.2 / §4.5). NOT an autonomous loop: a single,
/// governed step that lets a Prime (or the Founder) propose a team for
/// a strategy-approved Mandate and, where identities are supplied,
/// mint the (still pending-inert) hires under the spawn Key.
///
/// Wire arg: `mandate_id|description|roles` where `roles` is a CSV of
/// `role` or `role:subject_id` entries. A role with a `subject_id`
/// becomes a real pending hire (through the spawn gate — so a
/// lead/founder route mints a spawn Clearance); a bare role is only
/// *proposed* (no fabricated identity).
///
/// Governance:
/// - the Mandate strategy MUST be approved (else POLICY_DENIED);
/// - an Operative actor MUST hold the spawn Key (the assign Key is
///   reported as a readiness flag) — the Founder/Board bypasses.
///
/// Returns a stable JSON plan: `mandate_id`, `strategy_approved`,
/// `actor`, `description`, `proposed_roles`, `pending_hires`,
/// `clearances`, `denials`, `next_steps`. The plan itself is NOT yet
/// persisted as a Mandate Dossier — no Mandate-level document object
/// exists (documented gap); the caller receives the structured plan.
pub fn handle_team_plan(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("mandate.team_plan utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let mandate_id = parts.first().copied().unwrap_or("").trim();
    if mandate_id.is_empty() {
        return invalid("mandate.team_plan: expected `mandate_id|description?|roles?`".into());
    }
    let description = parts.get(1).copied().unwrap_or("").trim().to_string();
    let roles_csv = parts.get(2).copied().unwrap_or("");
    let tenant = ctx.tenant_id_or_default();

    // Gate 1: the Mandate strategy must be approved.
    match spine_store.strategy_approved(tenant, mandate_id) {
        Ok(true) => {}
        Ok(false) => {
            return policy_denied(format!(
                "mandate.team_plan: mandate `{mandate_id}` strategy is not approved"
            ));
        }
        Err(SpineStoreError::BadInput(m)) | Err(SpineStoreError::NotFound(m)) => {
            return invalid(format!("mandate.team_plan: {m}"));
        }
        Err(e) => return internal(format!("mandate.team_plan: {e}")),
    }

    // Gate 2: the actor's spawn Key (Founder/Board bypasses). This also
    // resolves the SpawnGate reused for every minted hire.
    let gate = match enforce_spawn_key(agent_store, ctx) {
        Ok(g) => g,
        Err(out) => return out,
    };
    let is_operator = caller_is_operator(ctx);
    let actor_label = if is_operator {
        "operator".to_string()
    } else {
        ctx.caller.subject_id.to_string()
    };
    // Readiness flag: can this actor also assign the work it staffs?
    let can_assign = if is_operator {
        true
    } else {
        agent_store
            .get_by_subject_for_tenant(&ctx.caller.subject_id.to_string(), tenant)
            .ok()
            .flatten()
            .map(|p| p.can_assign_work)
            .unwrap_or(false)
    };

    let mut proposed_roles: Vec<String> = Vec::new();
    let mut pending_hires: Vec<serde_json::Value> = Vec::new();
    let mut clearances: Vec<serde_json::Value> = Vec::new();
    let mut denials: Vec<serde_json::Value> = Vec::new();

    for entry in roles_csv
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let (role, subject) = match entry.split_once(':') {
            Some((r, sub)) => (r.trim(), sub.trim()),
            None => (entry, ""),
        };
        if role.is_empty() {
            continue;
        }
        if subject.is_empty() {
            // No identity supplied → proposed only (never fabricate one).
            proposed_roles.push(role.to_string());
            continue;
        }
        match agent_store.request_hire(
            role,
            role,
            role,
            "team",
            mandate_id,
            &actor_label,
            subject,
            "medium",
            tenant,
        ) {
            Ok(hire_id) => {
                pending_hires.push(serde_json::json!({
                    "role": role,
                    "agent_id": hire_id,
                    "subject_id": subject,
                }));
                if let SpawnGate::Clearance {
                    reason,
                    approver_subjects,
                } = &gate
                {
                    match agent_store.create_spawn_clearance(
                        &hire_id,
                        subject,
                        reason,
                        approver_subjects,
                        tenant,
                    ) {
                        Ok(cid) => clearances.push(serde_json::json!({
                            "agent_id": hire_id,
                            "clearance_id": cid,
                        })),
                        Err(e) => denials.push(serde_json::json!({
                            "role": role,
                            "reason": format!("spawn clearance: {e}"),
                        })),
                    }
                }
            }
            Err(AgentStoreError::BadInput(m)) => {
                denials.push(serde_json::json!({"role": role, "reason": m}));
            }
            Err(e) => {
                denials.push(serde_json::json!({"role": role, "reason": e.to_string()}));
            }
        }
    }

    // Honest next steps reflecting what was actually done.
    let mut next_steps: Vec<String> = Vec::new();
    if !clearances.is_empty() {
        next_steps.push(
            "Greenlight the spawn Clearances to activate the pending hires (coord.approval.decide / the Desk)."
                .to_string(),
        );
    }
    if !pending_hires.is_empty() && clearances.is_empty() {
        next_steps.push(
            "Approve the pending hires (agent.approve_hire) to bring them active.".to_string(),
        );
    }
    if !proposed_roles.is_empty() {
        next_steps.push(
            "Provide a subject_id for each proposed role (role:subject_id) to mint its hire."
                .to_string(),
        );
    }
    if !can_assign {
        next_steps.push(
            "Grant this actor can_assign_work to delegate Briefs to the new team.".to_string(),
        );
    }
    if next_steps.is_empty() {
        next_steps
            .push("Create Briefs under this Mandate and assign them to the team.".to_string());
    }

    // Coarse lifecycle status persisted on the plan row; the live
    // `mandate.team_readiness` view recomputes actual readiness.
    let clearance_ids: Vec<String> = clearances
        .iter()
        .filter_map(|c| c.get("clearance_id").and_then(|v| v.as_str()))
        .map(str::to_string)
        .collect();
    let status = if !clearance_ids.is_empty() {
        "awaiting_clearance"
    } else if !pending_hires.is_empty() {
        "staffing"
    } else {
        "planned"
    };

    // Persist the plan (durable artifact). Best-effort: the hires are
    // already minted, so a persistence error must not lose that work —
    // it is surfaced as `persisted: false` rather than failing the call.
    let to_json_str =
        |v: &serde_json::Value| serde_json::to_string(v).unwrap_or_else(|_| "[]".into());
    let proposed_json = serde_json::to_string(&proposed_roles).unwrap_or_else(|_| "[]".into());
    let pending_json = to_json_str(&serde_json::Value::Array(pending_hires.clone()));
    let clearance_ids_json = serde_json::to_string(&clearance_ids).unwrap_or_else(|_| "[]".into());
    let denials_json = to_json_str(&serde_json::Value::Array(denials.clone()));
    let next_steps_json = serde_json::to_string(&next_steps).unwrap_or_else(|_| "[]".into());
    let (plan_id, persisted) = match spine_store.record_team_plan(&TeamPlanRecord {
        tenant_id: tenant,
        mandate_id,
        actor_id: &actor_label,
        description: &description,
        proposed_roles_json: &proposed_json,
        pending_hires_json: &pending_json,
        clearance_ids_json: &clearance_ids_json,
        denials_json: &denials_json,
        next_steps_json: &next_steps_json,
        status,
    }) {
        Ok(id) => (Some(id), true),
        Err(e) => {
            tracing::warn!(mandate_id = %mandate_id, error = %e, "mandate.team_plan: persist failed");
            (None, false)
        }
    };

    let plan = serde_json::json!({
        "mandate_id": mandate_id,
        "plan_id": plan_id,
        "persisted": persisted,
        "status": status,
        "strategy_approved": true,
        "actor": actor_label,
        "description": description,
        "proposed_roles": proposed_roles,
        "pending_hires": pending_hires,
        "clearances": clearances,
        "clearance_ids": clearance_ids,
        "denials": denials,
        "next_steps": next_steps,
    });
    match serde_json::to_vec(&plan) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("mandate.team_plan encode: {e}")),
    }
}

/// `mandate.team_plan.latest` — the most recent persisted Team Plan
/// for a Mandate as JSON, or `{}`-ish `null` when none exists. Arg:
/// `mandate_id`. Tenant-scoped: a Mandate/plan from another Guild
/// reads as not-found.
pub fn handle_team_plan_latest(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let mandate_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.team_plan.latest utf8: {e}")),
    };
    if mandate_id.is_empty() {
        return invalid("mandate.team_plan.latest: mandate_id required".into());
    }
    match spine_store.latest_team_plan(ctx.tenant_id_or_default(), mandate_id) {
        Ok(Some(plan)) => match serde_json::to_vec(&plan.to_json()) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("mandate.team_plan.latest encode: {e}")),
        },
        // No plan yet → JSON null (the dashboard renders an empty state).
        Ok(None) => HandlerOutcome::Ok(b"null".to_vec()),
        Err(e) => internal(format!("mandate.team_plan.latest: {e}")),
    }
}

/// `mandate.team_readiness` — a live summary of how staffed a Mandate
/// is, combining the latest persisted Team Plan with the *current*
/// state of its hires and Clearances. Arg: `mandate_id`. Tenant-scoped.
///
/// This is what makes "the team is becoming active" visible without
/// mutating the plan row when a Clearance is approved: readiness is
/// recomputed on read from each hire's live status and each
/// Clearance's live status.
///
/// Returns JSON: `mandate_id`, `planned`, `plan_id`, `plan_status`,
/// `missing_roles` (proposed roles with no identity yet, plus roles
/// whose hire is disabled), `pending_clearances` (`{clearance_id,
/// status}`), `active_agents` (`{role, agent_id}` now active),
/// `pending_hires` (`{role, agent_id, status}` not yet active),
/// `blocked_roles` (`{role, reason}` from denials), `readiness`
/// (`not_planned` / `awaiting_clearance` / `staffing` / `ready`), and
/// `next_action`.
pub fn handle_team_readiness(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let mandate_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.team_readiness utf8: {e}")),
    };
    if mandate_id.is_empty() {
        return invalid("mandate.team_readiness: mandate_id required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    let plan = match spine_store.latest_team_plan(tenant, mandate_id) {
        Ok(p) => p,
        Err(e) => return internal(format!("mandate.team_readiness: {e}")),
    };
    let Some(plan) = plan else {
        // Never planned → an explicit not-planned readiness.
        let body = serde_json::json!({
            "mandate_id": mandate_id,
            "planned": false,
            "plan_id": serde_json::Value::Null,
            "plan_status": serde_json::Value::Null,
            "missing_roles": [],
            "pending_clearances": [],
            "active_agents": [],
            "pending_hires": [],
            "blocked_roles": [],
            "readiness": "not_planned",
            "next_action": "Plan a team for this Mandate (mandate.team_plan).",
        });
        return match serde_json::to_vec(&body) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("mandate.team_readiness encode: {e}")),
        };
    };

    let parse_arr = |s: &str| -> Vec<serde_json::Value> {
        serde_json::from_str::<Vec<serde_json::Value>>(s).unwrap_or_default()
    };
    let proposed_roles: Vec<String> = parse_arr(&plan.proposed_roles)
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let pending_hires = parse_arr(&plan.pending_hires);
    let clearance_ids: Vec<String> = parse_arr(&plan.clearance_ids)
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let denials = parse_arr(&plan.denials);

    // Classify each minted hire by its CURRENT status.
    let mut active_agents: Vec<serde_json::Value> = Vec::new();
    let mut pending_hires_out: Vec<serde_json::Value> = Vec::new();
    let mut missing_roles: Vec<String> = proposed_roles.clone();
    let mut blocked_roles: Vec<serde_json::Value> = denials.clone();
    for h in &pending_hires {
        let role = h.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let agent_id = h.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
        if agent_id.is_empty() {
            continue;
        }
        let status = agent_store
            .get_agent_for_tenant(agent_id, tenant)
            .ok()
            .flatten()
            .map(|p| p.status)
            .unwrap_or_else(|| "missing".to_string());
        match status.as_str() {
            "active" => active_agents.push(serde_json::json!({"role": role, "agent_id": agent_id})),
            "disabled" | "missing" => {
                // A rejected / vanished hire leaves the role unstaffed.
                missing_roles.push(role.to_string());
                blocked_roles.push(serde_json::json!({
                    "role": role,
                    "reason": format!("hire {agent_id} is {status}"),
                }));
            }
            other => pending_hires_out.push(serde_json::json!({
                "role": role, "agent_id": agent_id, "status": other,
            })),
        }
    }

    // Classify each Clearance by its CURRENT status.
    let mut pending_clearances: Vec<serde_json::Value> = Vec::new();
    for cid in &clearance_ids {
        let status = agent_store
            .get_approval_record_for_tenant(cid, tenant)
            .ok()
            .flatten()
            .map(|r| r.status.as_wire().to_string())
            .unwrap_or_else(|| "missing".to_string());
        if status == "pending" {
            pending_clearances.push(serde_json::json!({"clearance_id": cid, "status": status}));
        }
    }

    let (readiness, next_action): (&str, String) = if !pending_clearances.is_empty() {
        (
            "awaiting_clearance",
            format!(
                "Greenlight {} pending Clearance(s) to activate the hires.",
                pending_clearances.len()
            ),
        )
    } else if !pending_hires_out.is_empty() {
        (
            "staffing",
            format!(
                "Approve {} pending hire(s) (agent.approve_hire).",
                pending_hires_out.len()
            ),
        )
    } else if !missing_roles.is_empty() {
        (
            "staffing",
            format!(
                "Staff {} missing role(s) (mandate.team_plan with role:subject_id).",
                missing_roles.len()
            ),
        )
    } else if !active_agents.is_empty() {
        (
            "ready",
            "Team is active — create Briefs under this Mandate and assign them.".to_string(),
        )
    } else {
        (
            "staffing",
            "Add roles to the team (mandate.team_plan).".to_string(),
        )
    };

    let body = serde_json::json!({
        "mandate_id": mandate_id,
        "planned": true,
        "plan_id": plan.plan_id,
        "plan_status": plan.status,
        "missing_roles": missing_roles,
        "pending_clearances": pending_clearances,
        "active_agents": active_agents,
        "pending_hires": pending_hires_out,
        "blocked_roles": blocked_roles,
        "readiness": readiness,
        "next_action": next_action,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("mandate.team_readiness encode: {e}")),
    }
}

/// `brief.clearance_request` — create a real pending Clearance
/// linked to a Brief. Arg:
/// `brief_id|agent_id|method|category|reason|ttl_secs?`.
///
/// Used by the bridge-back HTTP surface when a thin Rig needs to ask
/// the Founder for permission mid-Shift. The subject id and approver
/// allowlist are derived from the stored Operative profile, not from
/// the caller's body.
pub fn handle_brief_clearance_request(
    agent_store: &AgentStore,
    task_store: &TaskStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("brief.clearance_request utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(6, '|').collect();
    if parts.len() < 5 {
        return invalid(
            "brief.clearance_request: expected `brief_id|agent_id|method|category|reason|ttl_secs?`"
                .into(),
        );
    }
    let brief_id = parts[0].trim();
    let agent_id = parts[1].trim();
    let method = parts[2].trim();
    let category = parts[3].trim();
    let reason = parts[4].trim();
    if brief_id.is_empty()
        || agent_id.is_empty()
        || method.is_empty()
        || category.is_empty()
        || reason.is_empty()
    {
        return invalid(
            "brief.clearance_request: brief_id, agent_id, method, category, and reason are required"
                .into(),
        );
    }
    let brief_fields = match task_store.brief_fields(brief_id) {
        Ok(Some(fields)) => fields,
        Ok(None) => {
            return invalid(format!(
                "brief.clearance_request: brief not found: {brief_id}"
            ));
        }
        Err(CoordinatorError::NotFound(_)) => {
            return invalid(format!(
                "brief.clearance_request: brief not found: {brief_id}"
            ));
        }
        Err(e) => return internal(format!("brief.clearance_request: brief lookup: {e}")),
    };
    if brief_fields.assignee_agent_id.as_deref() != Some(agent_id) {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!(
                "brief.clearance_request: agent `{agent_id}` is not assigned to Brief `{brief_id}`"
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let profile = match agent_store.get_agent(agent_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return invalid(format!(
                "brief.clearance_request: unknown agent: {agent_id}"
            ));
        }
        Err(e) => return internal(format!("brief.clearance_request: agent lookup: {e}")),
    };
    if profile.status != "active" {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!(
                "brief.clearance_request: agent `{agent_id}` is `{}`, not active",
                profile.status
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let ttl_secs = match parts.get(5).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        Some(raw) => match raw.parse::<i64>() {
            Ok(n) => n.clamp(30, 86_400),
            Err(_) => return invalid(format!("brief.clearance_request: bad ttl_secs: {raw}")),
        },
        None => profile.approval_timeout_secs.clamp(30, 86_400),
    };
    let expires_at = unix_now().saturating_add(ttl_secs);
    let hash = hex::encode(blake3::hash(ctx.args.as_slice()).as_bytes());
    let approval_id = match agent_store.create_approval(
        agent_id,
        &profile.subject_id,
        method,
        category,
        &hash,
        reason,
        &[],
        Some(brief_id),
        expires_at,
        &profile.authorized_approvers,
        ctx.tenant_id_or_default(),
    ) {
        Ok(id) => id,
        Err(AgentStoreError::BadInput(m)) => return invalid(m),
        Err(e) => return internal(format!("brief.clearance_request: {e}")),
    };
    if let Err(e) = task_store.update(
        brief_id,
        Some("awaiting_input"),
        None,
        None,
        None,
        None,
        None,
        None,
    ) {
        tracing::warn!(brief_id, approval_id = %approval_id, error = %e, "brief.clearance_request: awaiting_input update failed");
    }
    let payload = format!(
        "approval_id={approval_id}|agent_id={agent_id}|method={method}|category={category}"
    );
    if let Err(e) = task_store.append_event(brief_id, "brief.clearance_requested", &payload) {
        tracing::warn!(brief_id, approval_id = %approval_id, error = %e, "brief.clearance_request: chronicle event failed");
    }
    HandlerOutcome::Ok(format!("{approval_id}\n").into_bytes())
}

/// `agent.approve_hire` — approve a pending hire (pending → active).
/// Arg: agent_id.
pub fn handle_approve_hire(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.approve_hire utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.approve_hire: agent_id required".into());
    }
    match store.approve_hire(id) {
        Ok(()) => HandlerOutcome::Ok(b"approved\n".to_vec()),
        Err(AgentStoreError::NotFound(m)) => {
            invalid(format!("agent.approve_hire: not pending: {m}"))
        }
        Err(e) => internal(format!("agent.approve_hire: {e}")),
    }
}

/// `agent.reject_hire` — reject a pending hire (pending → disabled,
/// terminal). Arg: agent_id.
pub fn handle_reject_hire(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.reject_hire utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.reject_hire: agent_id required".into());
    }
    match store.reject_hire(id) {
        Ok(()) => HandlerOutcome::Ok(b"rejected\n".to_vec()),
        Err(AgentStoreError::NotFound(m)) => {
            invalid(format!("agent.reject_hire: not pending: {m}"))
        }
        Err(e) => internal(format!("agent.reject_hire: {e}")),
    }
}

// ── agent.get ────────────────────────────────────────────

pub fn handle_get(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.get utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.get: agent_id required".into());
    }
    match store.get_agent_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(Some(p)) => {
            let body = format!(
                "agent_id={}|name={}|role={}|title={}|department={}|team={}|created_by={}|status={}|subject_id={}|risk_ceiling={}|approval_timeout_secs={}|created_at={}|updated_at={}|surface_allowlist={}|allow_categories={}|deny_categories={}|allow_sensitivity_tags={}|deny_sensitivity_tags={}|approval_required_categories={}|rig={}|monthly_allowance_cents={}|max_concurrent_runs={}|wake_on_timer={}|wake_on_demand={}\n",
                p.agent_id,
                sanitize(&p.name),
                sanitize(&p.role),
                sanitize(&p.title),
                sanitize(&p.department),
                sanitize(&p.team),
                sanitize(&p.created_by),
                p.status,
                p.subject_id,
                p.risk_ceiling,
                p.approval_timeout_secs,
                p.created_at,
                p.updated_at,
                csv(&p.surface_allowlist),
                csv(&p.allow_categories),
                csv(&p.deny_categories),
                csv(&p.allow_sensitivity_tags),
                csv(&p.deny_sensitivity_tags),
                csv(&p.approval_required_categories),
                p.rig.as_deref().unwrap_or(""),
                p.monthly_allowance_cents
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                p.max_concurrent_runs,
                p.wake_on_timer,
                p.wake_on_demand,
            );
            HandlerOutcome::Ok(body.into_bytes())
        }
        Ok(None) => invalid(format!("agent.get: not found: {id}")),
        Err(e) => internal(format!("agent.get: {e}")),
    }
}

// ── agent.list ───────────────────────────────────────────

pub fn handle_list(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let arg = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.list utf8: {e}")),
    };
    let filter = if arg.is_empty() { None } else { Some(arg) };
    match store.list_agents(filter) {
        Ok(rows) => {
            let mut out = String::new();
            for r in &rows {
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    r.agent_id,
                    sanitize(&r.name),
                    sanitize(&r.role),
                    r.status,
                    r.subject_id,
                ));
            }
            out.push_str(&format!("count={}\n", rows.len()));
            HandlerOutcome::Ok(out.into_bytes())
        }
        Err(e) => internal(format!("agent.list: {e}")),
    }
}

// ── agent.update ─────────────────────────────────────────

pub fn handle_update(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.update utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    if parts.len() != 3 {
        return invalid("agent.update: expected `agent_id|field|value`".into());
    }
    // Configure-Key gate (company-model §5.2A): editing an Operative's
    // profile/Keys requires can_configure_agents (Founder/Board bypass;
    // self-config denied; tenant-scoped).
    if let Err(out) = enforce_configure_key(store, ctx, parts[0]) {
        return out;
    }
    match store.update_agent_field_for_tenant(
        parts[0],
        ctx.tenant_id_or_default(),
        parts[1],
        parts[2],
    ) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(AgentStoreError::NotFound(_)) => {
            invalid(format!("agent.update: not found: {}", parts[0]))
        }
        Err(AgentStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("agent.update: {e}")),
    }
}

// ── agent.delete (soft delete) ───────────────────────────

pub fn handle_delete(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.delete utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.delete: agent_id required".into());
    }
    // Configure-Key gate: disabling another Operative is a config
    // mutation (Founder/Board bypass; self-delete denied; tenant-scoped).
    if let Err(out) = enforce_configure_key(store, ctx, id) {
        return out;
    }
    match store.soft_delete_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(AgentStoreError::NotFound(_)) => invalid(format!("agent.delete: not found: {id}")),
        Err(e) => internal(format!("agent.delete: {e}")),
    }
}

// ── org tree (Roster / Lattice) reads ────────────────────

/// `agent.reports` — the Operatives directly reporting to `agent_id`
/// (the Roster children, one level down). Arg: agent_id. Returns one
/// agent_id per line.
pub fn handle_reports(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.reports utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.reports: agent_id required".into());
    }
    match store.list_direct_reports_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(rows) => HandlerOutcome::Ok(
            rows.into_iter()
                .map(|a| a.agent_id)
                .collect::<Vec<_>>()
                .join("\n")
                .into_bytes(),
        ),
        Err(e) => internal(format!("agent.reports: {e}")),
    }
}

/// `agent.by_role` — the active Operatives with a given role (the
/// assignable staff for that role). Arg: role. One agent_id per
/// line.
pub fn handle_by_role(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let role = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.by_role utf8: {e}")),
    };
    if role.is_empty() {
        return invalid("agent.by_role: role required".into());
    }
    match store.list_by_role_for_tenant(role, ctx.tenant_id_or_default()) {
        Ok(rows) => HandlerOutcome::Ok(rows.join("\n").into_bytes()),
        Err(e) => internal(format!("agent.by_role: {e}")),
    }
}

/// `agent.peers` — the Operatives reporting to the same Lead as
/// `agent_id` (excludes the agent itself). Arg: agent_id. One
/// agent_id per line; empty for an apex with no Lead.
pub fn handle_peers(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.peers utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.peers: agent_id required".into());
    }
    match store.list_peers_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(rows) => HandlerOutcome::Ok(rows.join("\n").into_bytes()),
        Err(e) => internal(format!("agent.peers: {e}")),
    }
}

/// `agent.branch` — every Operative at or below `agent_id` (the
/// manager's Branch / subtree, excluding the manager itself). The
/// delegated-authority scope. Arg: agent_id. One agent_id per line.
pub fn handle_branch(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.branch utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.branch: agent_id required".into());
    }
    match store.manager_subtree_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(ids) => HandlerOutcome::Ok(ids.join("\n").into_bytes()),
        Err(e) => internal(format!("agent.branch: {e}")),
    }
}

/// `agent.line` — the escalation path up from `agent_id` to the apex
/// (the Line / chain of command), nearest boss first. Arg: agent_id.
/// One agent_id per line; empty when the agent is the apex.
pub fn handle_line(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.line utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.line: agent_id required".into());
    }
    match store.chain_of_command_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(ids) => HandlerOutcome::Ok(ids.join("\n").into_bytes()),
        Err(e) => internal(format!("agent.line: {e}")),
    }
}

/// `agent.keys` — the full Operative profile as JSON: identity
/// (name/role/title/department/team/status), the **Keys** (the
/// permission surface — surface_allowlist, risk_ceiling,
/// allow/deny categories + sensitivity tags, approval-required
/// categories, authorized approvers, approval timeout, the
/// allow-all profile flag), and the **Lead** (reports_to). The
/// structured read backing the per-Operative Keys panel — a
/// JSON counterpart to the pipe-delimited `agent.get`. Arg:
/// agent_id.
pub fn handle_keys(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.keys utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.keys: agent_id required".into());
    }
    match store.get_agent_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(Some(p)) => match serde_json::to_vec(&p) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("agent.keys encode: {e}")),
        },
        Ok(None) => invalid(format!("agent.keys: not found: {id}")),
        Err(e) => internal(format!("agent.keys: {e}")),
    }
}

/// `agent.manages` — does `manager` manage `target` (target in
/// manager's Branch / subtree)? Arg `manager_id|target_id`. Returns
/// `true` / `false`. The delegated-authority check.
pub fn handle_manages(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.manages utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    if parts.len() < 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
        return invalid("agent.manages: expected `manager_id|target_id`".into());
    }
    match store.manages_for_tenant(parts[0].trim(), parts[1].trim(), ctx.tenant_id_or_default()) {
        Ok(b) => HandlerOutcome::Ok(if b {
            b"true".to_vec()
        } else {
            b"false".to_vec()
        }),
        Err(e) => internal(format!("agent.manages: {e}")),
    }
}

/// `agent.roster_summary` — Operative counts by status (+ `total`)
/// as JSON. No args. The Roster-at-a-glance for the companion /
/// dashboard.
pub fn handle_roster_summary(store: &AgentStore, _ctx: &InvocationCtx) -> HandlerOutcome {
    match store.status_counts() {
        Ok(counts) => {
            let mut obj = serde_json::Map::new();
            let mut total = 0i64;
            for (status, n) in counts {
                total += n;
                obj.insert(status, serde_json::Value::from(n));
            }
            obj.insert("total".to_string(), serde_json::Value::from(total));
            match serde_json::to_vec(&serde_json::Value::Object(obj)) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => internal(format!("agent.roster_summary encode: {e}")),
            }
        }
        Err(e) => internal(format!("agent.roster_summary: {e}")),
    }
}

/// `agent.allowance_committed` — total monthly Allowance committed
/// across the active roster, in cents (NULL counts as 0). No args.
/// Pairs with `guild.get` for commitment-vs-budget oversight.
pub fn handle_allowance_committed(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    match store.committed_allowance_cents_for_tenant(ctx.tenant_id_or_default()) {
        Ok(cents) => HandlerOutcome::Ok(cents.to_string().into_bytes()),
        Err(e) => internal(format!("agent.allowance_committed: {e}")),
    }
}

// ── agent.effective_capabilities ─────────────────────────

/// Wire arg: `agent_id|peer_alias`. The handler reaches into the
/// dispatch bridge's manifest cache for `peer_alias`'s capability
/// descriptors, intersects them against the agent's categorical
/// permissions, and returns the set of permitted methods. The
/// manifest reader is wired via the closure in `register`.
pub fn handle_effective_capabilities<F>(
    store: &AgentStore,
    ctx: &InvocationCtx,
    fetch_peer_methods: F,
) -> HandlerOutcome
where
    F: Fn(&str) -> Vec<(String, Vec<String>, Vec<String>, String)>,
{
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.effective_capabilities utf8: {e}")),
    };
    let (agent_id, peer_alias) = match s.split_once('|') {
        Some((a, p)) => (a.trim(), p.trim()),
        None => {
            return invalid("agent.effective_capabilities: expected `agent_id|peer_alias`".into());
        }
    };
    let agent = match store.get_agent(agent_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return invalid(format!(
                "agent.effective_capabilities: not found: {agent_id}"
            ));
        }
        Err(e) => return internal(format!("agent.effective_capabilities: {e}")),
    };
    if agent.status != "active" {
        // Disabled / suspended agents have zero effective
        // capabilities — be explicit rather than returning
        // the empty intersection silently.
        return HandlerOutcome::Ok(
            format!("count=0\nreason=agent_{}\n", agent.status).into_bytes(),
        );
    }
    let caps = fetch_peer_methods(peer_alias);
    let mut allowed = Vec::new();
    for (method, categories, sensitivity_tags, risk_level) in caps {
        if !risk_within_ceiling(&risk_level, &agent.risk_ceiling) {
            continue;
        }
        if categories
            .iter()
            .any(|c| agent.deny_categories.iter().any(|d| d == c))
        {
            continue;
        }
        if sensitivity_tags
            .iter()
            .any(|t| agent.deny_sensitivity_tags.iter().any(|d| d == t))
        {
            continue;
        }
        if !agent.allow_categories.is_empty()
            && !categories
                .iter()
                .any(|c| agent.allow_categories.iter().any(|a| a == c))
        {
            continue;
        }
        allowed.push(method);
    }
    allowed.sort();
    allowed.dedup();
    let mut out = String::new();
    for m in &allowed {
        out.push_str(m);
        out.push('\n');
    }
    out.push_str(&format!("count={}\n", allowed.len()));
    HandlerOutcome::Ok(out.into_bytes())
}

/// `safe < low < medium < high < critical`. Returns true when
/// `level <= ceiling`. Unknown levels are treated as exceeding
/// every ceiling (conservative default).
pub fn risk_within_ceiling(level: &str, ceiling: &str) -> bool {
    fn rank(s: &str) -> Option<i32> {
        match s {
            "safe" => Some(0),
            "low" => Some(1),
            "medium" => Some(2),
            "high" => Some(3),
            "critical" => Some(4),
            _ => None,
        }
    }
    match (rank(level), rank(ceiling)) {
        (Some(l), Some(c)) => l <= c,
        _ => false,
    }
}

// ── coord.approval.pending ───────────────────────────────

pub fn handle_approval_pending(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let arg = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("coord.approval.pending utf8: {e}")),
    };
    let limit: usize = if arg.is_empty() {
        20
    } else {
        match arg.parse() {
            Ok(n) => n,
            Err(_) => return invalid(format!("coord.approval.pending: bad limit: {arg}")),
        }
    };
    match store.list_pending_approvals_for_tenant(limit, ctx.tenant_id_or_default()) {
        Ok(rows) => {
            let mut out = String::new();
            for r in &rows {
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    r.approval_id,
                    r.agent_id,
                    r.method,
                    sanitize(&r.reason),
                    r.requested_at,
                ));
            }
            out.push_str(&format!("count={}\n", rows.len()));
            HandlerOutcome::Ok(out.into_bytes())
        }
        Err(e) => internal(format!("coord.approval.pending: {e}")),
    }
}

// ── coord.approval.get ───────────────────────────────────

/// DEFERRED 3 + DEFERRED C: per-approval status lookup.
///
/// Wire arg: raw `approval_id` bytes.
/// Wire response: JSON object with every operator-visible
/// field on the approval row. The bridge's
/// `GET /v1/approval/:id` route forwards the response verbatim;
/// the CLI prints `status` prominently and the rest as a JSON
/// dump under `--json`.
///
/// Fields:
///
/// - `approval_id`, `agent_id`, `subject_id` — caller binding.
/// - `method`, `capability_category`, `reason` — what was
///   requested + why.
/// - `requested_at`, `expires_at`, `decided_at` — lifecycle
///   timestamps in unix seconds.
/// - `status` — `pending` / `approved` / `rejected` / `expired`
///   / `consumed` / `legacy_token_expired`.
/// - `decided_by`, `decision_note` — operator attribution +
///   free-form note (sanitised; `decision_note` carries the
///   migration explanation when status is
///   `legacy_token_expired`).
/// - `task_id` — parked task (when present).
/// - `authorized_approvers` — the per-row allow-list the
///   `coord.approval.decide` cap enforces.
///
/// Returns `INVALID_ARGS` with cause "not found" when the id
/// is unknown — the bridge route maps that to HTTP 404 so
/// operator-facing tooling can distinguish missing-id from
/// real errors.
pub fn handle_approval_get(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("coord.approval.get utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("coord.approval.get: approval_id required".into());
    }
    match store.get_approval_record_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(Some(r)) => {
            let body = serde_json::json!({
                "approval_id": r.approval_id,
                "agent_id": r.agent_id,
                "subject_id": r.subject_id,
                "method": r.method,
                "capability_category": r.capability_category,
                "reason": r.reason,
                "requested_at": r.requested_at,
                "expires_at": r.expires_at,
                "status": r.status.as_wire(),
                "decided_at": r.decided_at,
                "decided_by": r.decided_by,
                "decision_note": r.decision_note,
                "task_id": r.task_id,
                "authorized_approvers": r.authorized_approvers,
            });
            match serde_json::to_vec(&body) {
                Ok(bytes) => HandlerOutcome::Ok(bytes),
                Err(e) => internal(format!("coord.approval.get: encode: {e}")),
            }
        }
        Ok(None) => invalid(format!("coord.approval.get: not found: {id}")),
        Err(e) => internal(format!("coord.approval.get: {e}")),
    }
}

// ── coord.approval.decide ────────────────────────────────

pub type TaskResumeFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// DEFERRED 2: roles that may decide ANY approval, regardless
/// of the per-row `authorized_approvers` allow-list. Stable
/// strings matched against `VerifiedIdentity.role` — kept in
/// lock-step with the matching constant in
/// `crate::approval::caps` so both decision surfaces share one
/// definition of "operator".
pub(crate) const OPERATOR_ROLES: &[&str] = &["operator", "admin"];

/// True when the verified caller is the Founder/Board (an
/// `operator` / `admin` role). This is the sovereign path
/// (company-model §5.4) that bypasses the per-Operative org/work
/// Keys — only an *agent*-originated call is gated by them.
pub(crate) fn caller_is_operator(ctx: &InvocationCtx) -> bool {
    OPERATOR_ROLES.contains(&ctx.caller.role.as_str())
}

/// Build a `POLICY_DENIED` outcome with a readable cause.
fn policy_denied(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::POLICY_DENIED,
        cause,
        retry_hint: 0,
        retry_after: None,
    })
}

/// Build a `SECURITY_DENIED` outcome with a readable cause.
fn security_denied(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::SECURITY_DENIED,
        cause,
        retry_hint: 0,
        retry_after: None,
    })
}

/// Outcome of the spawn-Key gate (company-model §5.2A).
pub(crate) enum SpawnGate {
    /// Founder/Board path, or an actor with `spawn_route=direct`: mint
    /// the pending-inert hire with no Clearance.
    Proceed,
    /// `spawn_route=lead|founder`: mint the pending-inert hire AND a
    /// typed spawn Clearance that must be greenlit to activate it.
    /// `approver_subjects` is the Lead's subject (route=lead) so that
    /// Lead may also decide; empty (route=founder) → operator/admin.
    Clearance {
        reason: String,
        approver_subjects: Vec<String>,
    },
}

/// Enforce the **spawn Key** (company-model §5.2A) for a hire that an
/// Operative actor originates. The Founder/Board bypasses; a denied
/// actor's outcome is returned verbatim. On `lead`/`founder` route the
/// caller must, after minting the pending hire, create the typed spawn
/// Clearance — see [`AgentStore::create_spawn_clearance`].
pub(crate) fn enforce_spawn_key(
    store: &AgentStore,
    ctx: &InvocationCtx,
) -> Result<SpawnGate, HandlerOutcome> {
    if caller_is_operator(ctx) {
        return Ok(SpawnGate::Proceed);
    }
    let tenant = ctx.tenant_id_or_default();
    let subject = ctx.caller.subject_id.to_string();
    let actor = match store.get_by_subject_for_tenant(&subject, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(security_denied(format!(
                "spawn denied: caller `{subject}` has no Operative profile in this Guild"
            )));
        }
        Err(e) => return Err(internal(format!("spawn key lookup: {e}"))),
    };
    match spawn_verdict(actor.can_spawn_agents, &actor.spawn_route) {
        KeyVerdict::Allow => Ok(SpawnGate::Proceed),
        KeyVerdict::Clearance { reason } => {
            // route=lead → the actor's Lead may decide (add its subject
            // to the approver allowlist); route=founder → leave empty so
            // only operator/admin decides. operator/admin is always
            // allowed regardless, so this only *widens* to the Lead.
            let mut approver_subjects = Vec::new();
            if super::keys::normalize_spawn_route(&actor.spawn_route) == "lead"
                && let Some(lead_id) = actor.reports_to.as_deref()
                && let Ok(Some(lead)) = store.get_agent_for_tenant(lead_id, tenant)
            {
                approver_subjects.push(lead.subject_id);
            }
            Ok(SpawnGate::Clearance {
                reason,
                approver_subjects,
            })
        }
        KeyVerdict::Deny { reason } => Err(policy_denied(format!("spawn denied: {reason}"))),
    }
}

/// After a pending hire is minted, finalise the spawn-Key outcome:
/// `Proceed` returns just the id; `Clearance` mints the **typed spawn
/// Clearance** linked to the pending hire (so approving it activates
/// the hire — see [`handle_approval_decide`]) and appends a
/// `clearance:` line carrying the new `clearance_id`.
fn finalize_spawn(
    store: &AgentStore,
    ctx: &InvocationCtx,
    hire_id: &str,
    hire_subject: &str,
    gate: SpawnGate,
) -> HandlerOutcome {
    let mut body = format!("{hire_id}\n");
    if let SpawnGate::Clearance {
        reason,
        approver_subjects,
    } = gate
    {
        match store.create_spawn_clearance(
            hire_id,
            hire_subject,
            &reason,
            &approver_subjects,
            ctx.tenant_id_or_default(),
        ) {
            Ok(cid) => body.push_str(&format!("clearance: {reason} (clearance_id={cid})\n")),
            Err(e) => return internal(format!("agent.request_hire spawn clearance: {e}")),
        }
    }
    HandlerOutcome::Ok(body.into_bytes())
}

/// Enforce assignment governance (company-model §5.2B / §5.3) for an
/// agent-originated Brief assignment to `assignee_id`. This is the
/// single chokepoint used by `brief.create` (initial assignee),
/// `brief.set` (assignee), and `brief.move` (into an execution state).
/// Returns:
///
/// - `Ok(())` — allowed, or bypassed (Founder/Board path; the assignee
///   value is empty i.e. the assignment is being *cleared*; or the
///   assignee is the actor itself i.e. claiming/working its own work).
/// - `Err(outcome)` — denied; the caller returns it verbatim.
///
/// Three layers, all tenant-scoped:
/// 1. **Actor identity** — a non-operator caller must have an
///    Operative profile in this Guild (else `SECURITY_DENIED`).
/// 2. **Assignee validity** — the assignee must exist in THIS Guild
///    and be `active`. This blocks cross-tenant / unknown ids and
///    disabled/pending/suspended hires from receiving executable work
///    (`POLICY_DENIED`).
/// 3. **Assign Key** — the actor's `can_assign_work` + `assign_scope`
///    must admit the assignee; Branch membership is resolved live from
///    the org tree (tenant-scoped).
pub(crate) fn enforce_assign_key(
    store: &AgentStore,
    ctx: &InvocationCtx,
    assignee_id: &str,
) -> Result<(), HandlerOutcome> {
    let assignee = assignee_id.trim();
    if assignee.is_empty() {
        // Clearing an assignee is not a grant of work.
        return Ok(());
    }
    if caller_is_operator(ctx) {
        // Founder/Board is sovereign over assignment authority.
        return Ok(());
    }
    let tenant = ctx.tenant_id_or_default();
    let subject = ctx.caller.subject_id.to_string();
    let actor = match store.get_by_subject_for_tenant(&subject, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(security_denied(format!(
                "assign denied: caller `{subject}` has no Operative profile in this Guild"
            )));
        }
        Err(e) => return Err(internal(format!("assign key lookup: {e}"))),
    };
    // Claiming / progressing your own work is not delegation.
    if assignee == actor.agent_id {
        return Ok(());
    }
    // Assignee validity: must exist in this Guild and be active.
    let target = match store.get_agent_for_tenant(assignee, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(policy_denied(format!(
                "assign denied: `{assignee}` is not an Operative in this Guild"
            )));
        }
        Err(e) => return Err(internal(format!("assignee lookup: {e}"))),
    };
    if target.status != "active" {
        return Err(policy_denied(format!(
            "assign denied: `{assignee}` is {} (not active) — cannot receive executable work",
            target.status
        )));
    }
    let in_branch = store
        .manages_for_tenant(&actor.agent_id, assignee, tenant)
        .unwrap_or(false);
    match assign_verdict(
        actor.can_assign_work,
        &actor.assign_scope,
        &actor.assign_allowed_agents,
        assignee,
        in_branch,
    ) {
        KeyVerdict::Allow => Ok(()),
        KeyVerdict::Clearance { reason } | KeyVerdict::Deny { reason } => {
            Err(policy_denied(format!("assign denied: {reason}")))
        }
    }
}

/// Enforce the **manage Key** (company-model §5.2A) for an
/// agent-originated mutation of a Brief **owned by another Operative**
/// (move / override fields / due / pin / snag). `owner` is the Brief's
/// current assignee. Returns `Ok(())` — bypassed — for: no owner (an
/// unowned Brief is not "another agent's work"), the Founder/Board
/// (operator/admin), and `owner == actor` (progressing one's own work
/// is normal execution, not management). Otherwise the actor needs
/// `can_manage_work` and a `manage_scope` that admits the owner. All
/// tenant-scoped; a disabled/pending/cross-tenant owner fails closed.
pub(crate) fn enforce_manage_key(
    store: &AgentStore,
    ctx: &InvocationCtx,
    owner: Option<&str>,
) -> Result<(), HandlerOutcome> {
    let Some(owner) = owner.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if caller_is_operator(ctx) {
        return Ok(());
    }
    let tenant = ctx.tenant_id_or_default();
    let subject = ctx.caller.subject_id.to_string();
    let actor = match store.get_by_subject_for_tenant(&subject, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(security_denied(format!(
                "manage denied: caller `{subject}` has no Operative profile in this Guild"
            )));
        }
        Err(e) => return Err(internal(format!("manage key lookup: {e}"))),
    };
    // Progressing your own assigned work is not management.
    if owner == actor.agent_id {
        return Ok(());
    }
    // The owner must be a real, active Operative in this Guild.
    let target = match store.get_agent_for_tenant(owner, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(policy_denied(format!(
                "manage denied: owner `{owner}` is not an Operative in this Guild"
            )));
        }
        Err(e) => return Err(internal(format!("manage owner lookup: {e}"))),
    };
    if target.status != "active" {
        return Err(policy_denied(format!(
            "manage denied: owner `{owner}` is {} (not active)",
            target.status
        )));
    }
    let in_branch = store
        .manages_for_tenant(&actor.agent_id, owner, tenant)
        .unwrap_or(false);
    match manage_verdict(
        actor.can_manage_work,
        &actor.manage_scope,
        &actor.manage_allowed_agents,
        owner,
        in_branch,
    ) {
        KeyVerdict::Allow => Ok(()),
        KeyVerdict::Clearance { reason } | KeyVerdict::Deny { reason } => {
            Err(policy_denied(format!("manage denied: {reason}")))
        }
    }
}

/// Enforce the **configure Key** (company-model §5.2A) for an
/// agent-originated mutation of `target_id`'s profile/Keys
/// (`agent.update` / `agent.delete`). Returns `Ok(())` — bypassed —
/// for the Founder/Board only. A non-operator actor:
/// - must have an Operative profile in this Guild (else
///   `SECURITY_DENIED`);
/// - may **not** configure *itself* — self-configuration is denied
///   outright to prevent privilege self-escalation (the company model
///   defines no safe self-config subset);
/// - needs `can_configure_agents` + a `configure_scope` that admits the
///   target; `configure_scope = none` denies even with the boolean on.
///
/// The target must exist in this Guild (cross-tenant fails closed).
pub(crate) fn enforce_configure_key(
    store: &AgentStore,
    ctx: &InvocationCtx,
    target_id: &str,
) -> Result<(), HandlerOutcome> {
    let target = target_id.trim();
    if caller_is_operator(ctx) {
        return Ok(());
    }
    let tenant = ctx.tenant_id_or_default();
    let subject = ctx.caller.subject_id.to_string();
    let actor = match store.get_by_subject_for_tenant(&subject, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(security_denied(format!(
                "configure denied: caller `{subject}` has no Operative profile in this Guild"
            )));
        }
        Err(e) => return Err(internal(format!("configure key lookup: {e}"))),
    };
    // No self-configuration: an Operative cannot edit its own profile /
    // Keys (would be self-escalation). Only the Founder/Board may.
    if target == actor.agent_id {
        return Err(policy_denied(
            "configure denied: an Operative cannot configure itself (self-escalation)".to_string(),
        ));
    }
    // The target must be a real Operative in this Guild (cross-tenant
    // and unknown ids fail closed).
    if store
        .get_agent_for_tenant(target, tenant)
        .map_err(|e| internal(format!("configure target lookup: {e}")))?
        .is_none()
    {
        return Err(policy_denied(format!(
            "configure denied: `{target}` is not an Operative in this Guild"
        )));
    }
    let in_branch = store
        .manages_for_tenant(&actor.agent_id, target, tenant)
        .unwrap_or(false);
    match configure_verdict(
        actor.can_configure_agents,
        &actor.configure_scope,
        &actor.configure_allowed_agents,
        target,
        in_branch,
    ) {
        KeyVerdict::Allow => Ok(()),
        KeyVerdict::Clearance { reason } | KeyVerdict::Deny { reason } => {
            Err(policy_denied(format!("configure denied: {reason}")))
        }
    }
}

/// `agent.assign_check` — would `actor` be permitted to assign a Brief
/// to `assignee` under its Keys? Arg `actor_id|assignee_id`. Returns
/// the JSON [`KeyVerdict`] (`{"decision":"allow"}` /
/// `{"decision":"deny","reason":…}`). The queryable counterpart to the
/// enforcement applied at `brief.set` — usable from the dashboard or a
/// manager Operative before it tries to delegate.
pub fn handle_assign_check(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.assign_check utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(2, '|').collect();
    if parts.len() != 2 {
        return invalid("agent.assign_check: expected `actor_id|assignee_id`".into());
    }
    let actor_id = parts[0].trim();
    let assignee_id = parts[1].trim();
    if actor_id.is_empty() || assignee_id.is_empty() {
        return invalid("agent.assign_check: actor_id and assignee_id required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    let actor = match store.get_agent_for_tenant(actor_id, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => return invalid(format!("agent.assign_check: not found: {actor_id}")),
        Err(e) => return internal(format!("agent.assign_check: {e}")),
    };
    let in_branch = match store.manages_for_tenant(actor_id, assignee_id, tenant) {
        Ok(b) => b,
        Err(e) => return internal(format!("agent.assign_check: {e}")),
    };
    let verdict = assign_verdict(
        actor.can_assign_work,
        &actor.assign_scope,
        &actor.assign_allowed_agents,
        assignee_id,
        in_branch,
    );
    match serde_json::to_vec(&verdict) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("agent.assign_check encode: {e}")),
    }
}

/// Default lifetime in seconds for a freshly-minted
/// [`crate::approval::ApprovalToken`]. Used as the fallback
/// when the operator did not configure
/// `[approval] approval_token_ttl_secs` in the controller
/// TOML. 5 minutes matches the spec's documented default.
///
/// DEFERRED 1: operators that need a longer / shorter TTL
/// override the value via the config field. The runtime
/// clamps the configured value to
/// `[APPROVAL_TOKEN_TTL_MIN_SECS, APPROVAL_TOKEN_TTL_MAX_SECS]`
/// at boot so a typo cannot mint forever-tokens or
/// instantly-expired tokens.
pub const APPROVAL_TOKEN_TTL_DEFAULT_SECS: u64 = 5 * 60;

/// Minimum allowed token TTL after operator-config clamping.
/// 30 seconds is the floor a real operator can vote within;
/// values below this almost always indicate a misconfigured
/// unit (seconds vs. milliseconds).
pub const APPROVAL_TOKEN_TTL_MIN_SECS: u64 = 30;

/// Maximum allowed token TTL after operator-config clamping.
/// 24 hours is the spec's documented ceiling — anything longer
/// turns the one-shot token into an effective long-lived
/// credential, defeating the purpose of binding to a single
/// approval.
pub const APPROVAL_TOKEN_TTL_MAX_SECS: u64 = 24 * 60 * 60;

/// Back-compat alias for callers that want the default TTL in
/// milliseconds. New code should call
/// [`clamp_approval_token_ttl_secs`] on the configured value and
/// multiply by 1000 at the mint site.
pub const APPROVAL_TOKEN_TTL_MS: i64 = (APPROVAL_TOKEN_TTL_DEFAULT_SECS as i64) * 1000;

/// DEFERRED 1: clamp an operator-supplied TTL (in seconds) to
/// the allowed `[MIN, MAX]` window. `None` returns the default.
/// Pure function — exposed so the controller startup logs the
/// effective value and tests pin the contract.
pub fn clamp_approval_token_ttl_secs(configured: Option<u64>) -> u64 {
    configured
        .unwrap_or(APPROVAL_TOKEN_TTL_DEFAULT_SECS)
        .clamp(APPROVAL_TOKEN_TTL_MIN_SECS, APPROVAL_TOKEN_TTL_MAX_SECS)
}

/// Wire arg: `approval_id|decision|decided_by|note`.
/// `decision` is `approved` or `rejected`.
/// On `approved`, returns `ok|<wire_token>\n` (where
/// `<wire_token>` is the structured base64url-encoded
/// [`crate::approval::ApprovalToken`]) and calls `resume_task`
/// to flip the waiting task back to `running`. On `rejected`,
/// returns `ok\n` and calls `fail_task`.
///
/// P1: `signer` is the Ed25519 signer the cap handler signs
/// the token with. The controller wires it from
/// `RELIX_APPROVAL_SIGNING_KEY` at startup. `None` means "no
/// signer configured" — the decision still completes (status
/// flips on the row) but no token is returned, so operators see
/// `ok\n` and the caller cannot mint admission-time proof.
/// Fail-loud: the controller logs the missing env var at boot.
///
/// DEFERRED 1: `token_ttl_secs` is the operator-configured TTL
/// AFTER controller-startup clamping via
/// [`clamp_approval_token_ttl_secs`]. The handler does not
/// re-clamp — the caller MUST already have done so. Passing an
/// out-of-range value is a caller bug, not a security issue.
///
/// NOT-DONE 1: `clock` is the injected time source for
/// `issued_at_ms`. Production wires
/// [`relix_core::clock::SystemClock`]; tests wire
/// [`relix_core::clock::FakeClock`] so the mint timestamp is
/// deterministic.
pub fn handle_approval_decide(
    store: &AgentStore,
    ctx: &InvocationCtx,
    resume_task: &TaskResumeFn,
    fail_task: &TaskResumeFn,
    signer: Option<&crate::approval::ApprovalSigner>,
    token_ttl_secs: u64,
    clock: &dyn relix_core::clock::Clock,
) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("coord.approval.decide utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(4, '|').collect();
    if parts.len() < 3 {
        return invalid(
            "coord.approval.decide: expected `approval_id|decision|decided_by|note?`".into(),
        );
    }
    let approval_id = parts[0].trim();
    let decision_raw = parts[1].trim();
    let decided_by = parts[2];
    let note = parts.get(3).copied().unwrap_or("");
    let decision = match decision_raw {
        "approved" => ApprovalStatus::Approved,
        "rejected" => ApprovalStatus::Rejected,
        other => return invalid(format!("coord.approval.decide: bad decision: {other}")),
    };
    // Capture the task_id BEFORE deciding so we can resume / fail
    // on the right row even if the decide call writes the
    // terminal state first.
    // GROUP 6 (tenant isolation): only this Guild's approval is
    // visible — a known approval_id from another tenant resolves to
    // not-found, so it can be neither read nor decided cross-tenant.
    let record = match store.get_approval_record_for_tenant(approval_id, ctx.tenant_id_or_default())
    {
        Ok(Some(r)) => r,
        Ok(None) => return invalid(format!("coord.approval.decide: not found: {approval_id}")),
        Err(e) => return internal(format!("coord.approval.decide: {e}")),
    };
    // DEFERRED 2: authorised-approver check. The cap admits the
    // caller iff:
    //   1. the caller's verified subject_id is in
    //      `record.authorized_approvers`, OR
    //   2. the caller's verified role is in OPERATOR_ROLES
    //      (operator / admin).
    // Wire-typed `decided_by` is the operator's typed-by-hand
    // display name; admission is keyed off the cryptographically
    // verified `ctx.caller` instead.
    let caller_subject = ctx.caller.subject_id.to_string();
    let caller_role = ctx.caller.role.as_str();
    let role_admits = OPERATOR_ROLES.contains(&caller_role);
    let listed = record
        .authorized_approvers
        .iter()
        .any(|s| s == &caller_subject);
    if !role_admits && !listed {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::SECURITY_DENIED,
            cause: format!(
                "coord.approval.decide: caller `{caller_subject}` is not an \
                 authorised approver for `{approval_id}` (role={caller_role})"
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let task_id = record.task_id.clone();
    let metadata = match store.decide_approval(approval_id, decision, decided_by, note) {
        Ok(t) => t,
        Err(AgentStoreError::NotFound(_)) => {
            return invalid(format!("coord.approval.decide: not found: {approval_id}"));
        }
        Err(AgentStoreError::BadInput(m)) => return invalid(m),
        Err(e) => return internal(format!("coord.approval.decide: {e}")),
    };
    if let Some(tid) = task_id.as_deref() {
        let r = match decision {
            ApprovalStatus::Approved => resume_task(tid),
            ApprovalStatus::Rejected => fail_task(tid),
            _ => Ok(()),
        };
        if let Err(e) = r {
            tracing::warn!(task_id = %tid, error = %e, "coord.approval.decide: task hop failed");
        }
    }
    // Spawn-Clearance hop (company-model §5.2A): when this approval is
    // the typed spawn Clearance for a pending hire, approving it
    // *activates* the hire (pending → active) and rejecting it disables
    // it. The hire stays inert until this fires. `record.agent_id` is
    // the pending hire; the row already passed the tenant + approver
    // checks above, so acting by agent_id is safe.
    if record.method == crate::nodes::coordinator::agent::store::SPAWN_CLEARANCE_METHOD {
        let hire_id = record.agent_id.as_str();
        let r = match decision {
            ApprovalStatus::Approved => store.approve_hire(hire_id),
            ApprovalStatus::Rejected => store.reject_hire(hire_id),
            _ => Ok(()),
        };
        if let Err(e) = r {
            tracing::warn!(
                agent_id = %hire_id,
                error = %e,
                "coord.approval.decide: spawn-clearance hire hop failed (already decided?)"
            );
        }
    }
    // P1: mint the Ed25519-signed token when the approval was
    // approved. The legacy `ok|<random>\n` wire shape is
    // preserved — only the contents change to a
    // base64url(json)-encoded Ed25519 `ApprovalToken`.
    let body = match metadata {
        Some(meta) if signer.is_some() => {
            let signer = signer.expect("checked above");
            // Source `issued_at_ms` from the injected clock so
            // the token's TTL window is deterministic under
            // test.
            let issued_at_ms = clock.now_ms();
            let ttl_ms = (token_ttl_secs as i64).saturating_mul(1000);
            match crate::approval::ApprovalToken::issue(
                &meta.approval_id,
                &meta.method,
                &meta.subject_id,
                meta.task_id.as_deref().unwrap_or(""),
                issued_at_ms,
                ttl_ms,
                signer,
            ) {
                Ok(wire) => format!("ok|{wire}\n"),
                Err(e) => {
                    tracing::error!(
                        approval_id = %meta.approval_id,
                        error = %e,
                        "coord.approval.decide: token mint failed"
                    );
                    return internal(format!("coord.approval.decide: token mint: {e}"));
                }
            }
        }
        Some(meta) => {
            tracing::warn!(
                approval_id = %meta.approval_id,
                "coord.approval.decide: Ed25519 signer not configured; approving without token"
            );
            "ok\n".to_string()
        }
        None => "ok\n".to_string(),
    };
    HandlerOutcome::Ok(body.into_bytes())
}

// ── standing approval handlers ──────────────────────────

#[derive(Debug, Deserialize)]
struct StandingCreateJson {
    agent_id: String,
    #[serde(alias = "category")]
    match_category: String,
    expires_at: i64,
    #[serde(default)]
    granted_by: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default, alias = "path_glob")]
    match_path_glob: Option<String>,
    #[serde(default)]
    scope_kind: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    method_prefix: Option<String>,
    #[serde(default)]
    workspace_path_glob: Option<String>,
    #[serde(default)]
    max_calls: Option<i64>,
    #[serde(default)]
    max_cost_micros: Option<i64>,
}

/// Legacy wire arg:
/// `agent_id|category|expires_at|granted_by|note|path_glob?`
///
/// Scoped wire arg:
/// JSON object containing `agent_id`, `category`/`match_category`,
/// `expires_at`, plus optional `scope_kind`, `task_id`,
/// `session_id`, `method_prefix`, and `workspace_path_glob`.
pub fn handle_standing_create(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.standing_approval.create utf8: {e}")),
    };
    if s.trim_start().starts_with('{') {
        let req: StandingCreateJson = match serde_json::from_str(s) {
            Ok(req) => req,
            Err(e) => {
                return invalid(format!("agent.standing_approval.create json: {e}"));
            }
        };
        let granted_by = req.granted_by.as_deref().unwrap_or("operator");
        let note = req.note.as_deref().unwrap_or("");
        return match store.create_scoped_standing(StandingApprovalCreate {
            agent_id: &req.agent_id,
            match_category: &req.match_category,
            match_path_glob: req.match_path_glob.as_deref(),
            scope_kind: req.scope_kind.as_deref(),
            task_id: req.task_id.as_deref(),
            session_id: req.session_id.as_deref(),
            method_prefix: req.method_prefix.as_deref(),
            workspace_path_glob: req.workspace_path_glob.as_deref(),
            expires_at: req.expires_at,
            granted_by,
            max_calls: req.max_calls,
            max_cost_micros: req.max_cost_micros,
            note,
            tenant_id: ctx.tenant_id_or_default(),
        }) {
            Ok(id) => HandlerOutcome::Ok(format!("{id}\n").into_bytes()),
            Err(AgentStoreError::BadInput(m)) => invalid(m),
            Err(e) => internal(format!("agent.standing_approval.create: {e}")),
        };
    }
    let parts: Vec<&str> = s.splitn(6, '|').collect();
    if parts.len() < 5 {
        return invalid(
            "agent.standing_approval.create: expected `agent_id|category|expires_at|granted_by|note|path_glob?`"
                .into(),
        );
    }
    let agent_id = parts[0].trim();
    let category = parts[1].trim();
    let expires_at: i64 = match parts[2].trim().parse() {
        Ok(n) => n,
        Err(_) => {
            return invalid(format!(
                "agent.standing_approval.create: bad expires_at: {}",
                parts[2]
            ));
        }
    };
    let granted_by = parts[3].trim();
    let note = parts[4];
    let path_glob = parts.get(5).and_then(|p| {
        let t = p.trim();
        if t.is_empty() { None } else { Some(t) }
    });
    match store.create_standing(
        agent_id,
        category,
        path_glob,
        expires_at,
        granted_by,
        note,
        ctx.tenant_id_or_default(),
    ) {
        Ok(id) => HandlerOutcome::Ok(format!("{id}\n").into_bytes()),
        Err(AgentStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("agent.standing_approval.create: {e}")),
    }
}

pub fn handle_standing_list(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let agent_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.standing_approval.list utf8: {e}")),
    };
    if agent_id.is_empty() {
        return invalid("agent.standing_approval.list: agent_id required".into());
    }
    match store.list_standing_for_tenant(agent_id, ctx.tenant_id_or_default()) {
        Ok(rows) => {
            let mut out = String::new();
            for r in &rows {
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    r.standing_id,
                    r.match_category,
                    r.match_path_glob.as_deref().unwrap_or(""),
                    r.scope_kind,
                    r.task_id.as_deref().unwrap_or(""),
                    r.session_id.as_deref().unwrap_or(""),
                    r.method_prefix.as_deref().unwrap_or(""),
                    r.workspace_path_glob.as_deref().unwrap_or(""),
                    r.expires_at,
                    r.granted_by,
                    r.max_calls.map(|n| n.to_string()).unwrap_or_default(),
                    r.calls_used,
                    r.max_cost_micros.map(|n| n.to_string()).unwrap_or_default(),
                    r.cost_used_micros,
                    sanitize(&r.note)
                ));
            }
            out.push_str(&format!("count={}\n", rows.len()));
            HandlerOutcome::Ok(out.into_bytes())
        }
        Err(e) => internal(format!("agent.standing_approval.list: {e}")),
    }
}

pub fn handle_standing_revoke(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let standing_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.standing_approval.revoke utf8: {e}")),
    };
    if standing_id.is_empty() {
        return invalid("agent.standing_approval.revoke: standing_id required".into());
    }
    match store.revoke_standing(standing_id) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(AgentStoreError::NotFound(_)) => invalid(format!(
            "agent.standing_approval.revoke: not found: {standing_id}"
        )),
        Err(e) => internal(format!("agent.standing_approval.revoke: {e}")),
    }
}

// ── helpers ──────────────────────────────────────────────

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

fn sanitize(s: &str) -> String {
    s.replace('|', " ").replace(['\n', '\r', '\t'], " ")
}

fn csv(v: &[String]) -> String {
    v.join(",")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Re-export so the executor module and tests can reach for
/// the canonical default category list without re-importing
/// from the store module.
pub fn default_approval_required_categories() -> Vec<String> {
    default_approval_categories()
}

#[cfg(test)]
pub(crate) fn fake_ctx(args: &[u8]) -> InvocationCtx {
    fake_ctx_with_role(args, "operator", b"caller")
}

/// DEFERRED 2: parameterised test-context builder. `fake_ctx`
/// keeps the default `operator` role so the existing handler
/// tests pass the new SEC PART B authorised-approver check at
/// `coord.approval.decide`; deny-path tests use this helper
/// directly with role = `"agent"` (or another non-operator
/// role).
#[cfg(test)]
pub(crate) fn fake_ctx_with_role(args: &[u8], role: &str, subject_seed: &[u8]) -> InvocationCtx {
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};
    InvocationCtx {
        caller: VerifiedIdentity {
            subject_id: NodeId::from_pubkey(subject_seed),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(b"org"),
            groups: vec![],
            role: role.into(),
            clearance: String::new(),
            bundle_id: [0; 32],
        },
        trace_id: TraceId::new(),
        request_id: RequestId::new(),
        args: args.to_vec(),
        tenant_id: None,
    }
}

/// Operator-role ctx carrying an explicit verified tenant — used to
/// prove the product agent routes scope by tenant.
#[cfg(test)]
pub(crate) fn fake_ctx_tenant(args: &[u8], tenant: &str) -> InvocationCtx {
    let mut c = fake_ctx_with_role(args, "operator", b"caller");
    c.tenant_id = Some(tenant.to_string());
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> AgentStore {
        AgentStore::in_memory().unwrap()
    }

    fn ok_body(o: HandlerOutcome) -> String {
        match o {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok: {} {}", e.kind, e.cause),
        }
    }
    fn err_kind(o: HandlerOutcome) -> u32 {
        match o {
            HandlerOutcome::Ok(_) => panic!("expected Err"),
            HandlerOutcome::Err(e) => e.kind,
        }
    }

    #[test]
    fn create_handler_returns_agent_id() {
        let s = store();
        let out = handle_create(
            &s,
            &fake_ctx(b"Research|research|Junior|research|ops|alice|subj-1|medium"),
        );
        let id = ok_body(out).trim().to_string();
        assert!(id.starts_with("agt_research_"));
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.name, "Research");
    }

    #[test]
    fn create_handler_rejects_wrong_pipe_count() {
        let s = store();
        let out = handle_create(&s, &fake_ctx(b"too|few|fields"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn request_hire_for_mandate_requires_approved_strategy() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let mandate = spine
            .create_mandate("default", "Ship v1", "make the product real", None, None)
            .unwrap();
        let arg = format!("{mandate}|Planner|planner|Planner|ops|ops|prime|subj-plan|medium");

        let out = handle_request_hire_for_mandate(&agents, &spine, &fake_ctx(arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);

        spine
            .propose_strategy("default", &mandate, "hire planner; assign briefs")
            .unwrap();
        let out = handle_request_hire_for_mandate(&agents, &spine, &fake_ctx(arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
    }

    #[test]
    fn request_hire_for_mandate_creates_pending_hire_after_approval() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let mandate = spine
            .create_mandate("default", "Ship v1", "make the product real", None, None)
            .unwrap();
        spine
            .propose_strategy("default", &mandate, "hire planner; assign briefs")
            .unwrap();
        spine.approve_strategy("default", &mandate).unwrap();

        let arg = format!("{mandate}|Planner|planner|Planner|ops|ops|prime|subj-plan|medium");
        let id = ok_body(handle_request_hire_for_mandate(
            &agents,
            &spine,
            &fake_ctx(arg.as_bytes()),
        ))
        .trim()
        .to_string();
        let hire = agents.get_agent(&id).unwrap().unwrap();
        assert_eq!(hire.status, "pending");
        assert_eq!(hire.name, "Planner");
    }

    #[test]
    fn mandate_founder_route_mints_spawn_clearance_only_after_strategy_approval() {
        use crate::nodes::coordinator::agent::store::SPAWN_CLEARANCE_METHOD;
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let mandate = spine
            .create_mandate("default", "Ship v1", "real product", None, None)
            .unwrap();
        // An agent actor with can_spawn + founder route.
        let actor = agents
            .create_agent(
                "Prime",
                "prime",
                "Lead",
                "ops",
                "ops",
                "founder",
                &subject_of(b"planner-seed"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&actor, "can_spawn_agents", "true")
            .unwrap();
        let arg = format!("{mandate}|Worker|engineer|W|eng|eng|prime|subj-w|medium");
        // Strategy NOT approved → refused, no hire, no Clearance.
        let out = handle_request_hire_for_mandate(
            &agents,
            &spine,
            &fake_ctx_with_role(arg.as_bytes(), "prime", b"planner-seed"),
        );
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
        assert!(agents.list_pending_approvals(100).unwrap().is_empty());
        // Approve strategy → now the hire + spawn Clearance are minted.
        spine
            .propose_strategy("default", &mandate, "hire a worker")
            .unwrap();
        spine.approve_strategy("default", &mandate).unwrap();
        let body = ok_body(handle_request_hire_for_mandate(
            &agents,
            &spine,
            &fake_ctx_with_role(arg.as_bytes(), "prime", b"planner-seed"),
        ));
        let hire_id = body.lines().next().unwrap().trim();
        assert_eq!(
            agents.get_agent(hire_id).unwrap().unwrap().status,
            "pending"
        );
        assert!(
            agents
                .list_pending_approvals(100)
                .unwrap()
                .iter()
                .any(|r| r.method == SPAWN_CLEARANCE_METHOD && r.agent_id == hire_id),
            "strategy-approved founder-route mandate hire must mint a spawn Clearance"
        );
    }

    // ── Prime team-build foundation (mandate.team_plan) ──────

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

    #[test]
    fn team_plan_refuses_unapproved_strategy() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate("default", "Ship v1", "real product", None, None)
            .unwrap();
        let arg = format!("{m}|grow|planner");
        let out = handle_team_plan(&agents, &spine, &fake_ctx(arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
    }

    #[test]
    fn team_plan_refuses_actor_without_spawn_key() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // Agent actor that exists but lacks can_spawn_agents.
        agents
            .create_agent(
                "Prime",
                "prime",
                "L",
                "ops",
                "ops",
                "founder",
                &subject_of(b"planner-seed"),
                "medium",
                "default",
            )
            .unwrap();
        let arg = format!("{m}|grow|planner");
        let out = handle_team_plan(
            &agents,
            &spine,
            &fake_ctx_with_role(arg.as_bytes(), "prime", b"planner-seed"),
        );
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
    }

    #[test]
    fn team_plan_operator_proposes_roles_and_mints_identified_hires() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // Founder/operator path: one bare role (proposed) + one with a
        // subject_id (minted as a pending hire).
        let arg = format!("{m}|grow the team|planner,engineer:subj-eng");
        let body = ok_body(handle_team_plan(&agents, &spine, &fake_ctx(arg.as_bytes())));
        let v: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
        // Stable JSON shape.
        assert_eq!(v["mandate_id"], m);
        assert_eq!(v["strategy_approved"], true);
        assert_eq!(v["actor"], "operator");
        assert_eq!(v["description"], "grow the team");
        assert_eq!(v["proposed_roles"], serde_json::json!(["planner"]));
        let hires = v["pending_hires"].as_array().unwrap();
        assert_eq!(hires.len(), 1);
        assert_eq!(hires[0]["role"], "engineer");
        assert_eq!(hires[0]["subject_id"], "subj-eng");
        // Operator path mints no spawn Clearance (hires await approve_hire).
        assert!(v["clearances"].as_array().unwrap().is_empty());
        // The minted hire is real + pending-inert.
        let hire_id = hires[0]["agent_id"].as_str().unwrap();
        assert_eq!(
            agents.get_agent(hire_id).unwrap().unwrap().status,
            "pending"
        );
        assert!(v["next_steps"].as_array().unwrap().iter().count() >= 1);
    }

    #[test]
    fn team_plan_founder_route_actor_mints_hire_with_spawn_clearance() {
        use crate::nodes::coordinator::agent::store::SPAWN_CLEARANCE_METHOD;
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let actor = agents
            .create_agent(
                "Prime",
                "prime",
                "L",
                "ops",
                "ops",
                "founder",
                &subject_of(b"planner-seed"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&actor, "can_spawn_agents", "true")
            .unwrap();
        // spawn_route stays 'founder' → identified hires get a Clearance.
        let arg = format!("{m}|build|engineer:subj-eng");
        let body = ok_body(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx_with_role(arg.as_bytes(), "prime", b"planner-seed"),
        ));
        let v: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
        let hire_id = v["pending_hires"][0]["agent_id"].as_str().unwrap();
        let clearances = v["clearances"].as_array().unwrap();
        assert_eq!(
            clearances.len(),
            1,
            "founder route must mint a spawn Clearance"
        );
        assert_eq!(clearances[0]["agent_id"], hire_id);
        // And the Clearance is a real pending approval tied to the hire.
        assert!(
            agents
                .list_pending_approvals(100)
                .unwrap()
                .iter()
                .any(|r| r.method == SPAWN_CLEARANCE_METHOD && r.agent_id == hire_id)
        );
    }

    #[test]
    fn team_plan_persists_and_latest_round_trips() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let arg = format!("{m}|grow|planner,engineer:subj-eng");
        let body = ok_body(handle_team_plan(&agents, &spine, &fake_ctx(arg.as_bytes())));
        let v: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
        assert_eq!(v["persisted"], true);
        assert!(v["plan_id"].as_str().is_some());
        // status reflects a pending hire with no clearance (operator path).
        assert_eq!(v["status"], "staffing");

        // The persisted latest plan round-trips the same content.
        let latest = ok_body(handle_team_plan_latest(&spine, &fake_ctx(m.as_bytes())));
        let lv: serde_json::Value = serde_json::from_slice(latest.as_bytes()).unwrap();
        assert_eq!(lv["mandate_id"], m);
        assert_eq!(lv["proposed_roles"], serde_json::json!(["planner"]));
        assert_eq!(lv["pending_hires"][0]["role"], "engineer");
        assert_eq!(lv["status"], "staffing");
        assert_eq!(lv["plan_id"], v["plan_id"]);
    }

    #[test]
    fn team_plan_latest_is_none_until_planned() {
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let body = ok_body(handle_team_plan_latest(&spine, &fake_ctx(m.as_bytes())));
        assert_eq!(body.trim(), "null");
    }

    #[test]
    fn team_readiness_not_planned_until_a_plan_exists() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let body = ok_body(handle_team_readiness(
            &agents,
            &spine,
            &fake_ctx(m.as_bytes()),
        ));
        let v: serde_json::Value = serde_json::from_slice(body.as_bytes()).unwrap();
        assert_eq!(v["planned"], false);
        assert_eq!(v["readiness"], "not_planned");
    }

    #[test]
    fn team_readiness_reflects_clearance_approval_activating_the_hire() {
        // Plan via a founder-route actor → a pending hire + spawn
        // Clearance. Readiness starts `awaiting_clearance`; approving
        // the Clearance activates the hire and readiness becomes `ready`.
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let actor = agents
            .create_agent(
                "Prime",
                "prime",
                "L",
                "ops",
                "ops",
                "founder",
                &subject_of(b"planner-seed"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&actor, "can_spawn_agents", "true")
            .unwrap();
        let arg = format!("{m}|build|engineer:subj-eng");
        ok_body(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx_with_role(arg.as_bytes(), "prime", b"planner-seed"),
        ));
        // Before deciding: awaiting_clearance with one pending clearance.
        let before: serde_json::Value = serde_json::from_slice(
            ok_body(handle_team_readiness(
                &agents,
                &spine,
                &fake_ctx(m.as_bytes()),
            ))
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(before["readiness"], "awaiting_clearance");
        assert_eq!(before["pending_clearances"].as_array().unwrap().len(), 1);
        assert!(before["active_agents"].as_array().unwrap().is_empty());
        // Approve the spawn Clearance (Founder/operator path).
        let cid = before["pending_clearances"][0]["clearance_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(matches!(
            decide(&agents, &cid, "approved"),
            HandlerOutcome::Ok(_)
        ));
        // After: the hire is active → readiness is ready.
        let after: serde_json::Value = serde_json::from_slice(
            ok_body(handle_team_readiness(
                &agents,
                &spine,
                &fake_ctx(m.as_bytes()),
            ))
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(after["readiness"], "ready");
        assert_eq!(after["active_agents"].as_array().unwrap().len(), 1);
        assert!(after["pending_clearances"].as_array().unwrap().is_empty());
    }

    #[test]
    fn team_readiness_is_tenant_isolated() {
        // A plan in tenant A must not surface as readiness for tenant B.
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate("tenant-a", "Ship", "real", None, None)
            .unwrap();
        spine.propose_strategy("tenant-a", &m, "plan").unwrap();
        spine.approve_strategy("tenant-a", &m).unwrap();
        ok_body(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx_tenant(format!("{m}|grow|planner").as_bytes(), "tenant-a"),
        ));
        let b: serde_json::Value = serde_json::from_slice(
            ok_body(handle_team_readiness(
                &agents,
                &spine,
                &fake_ctx_tenant(m.as_bytes(), "tenant-b"),
            ))
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(b["planned"], false, "tenant B must not see tenant A's plan");
    }

    #[test]
    fn team_plan_latest_is_tenant_isolated() {
        // Tenant A plans a team; tenant B must not read it.
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate("tenant-a", "Ship", "real", None, None)
            .unwrap();
        spine.propose_strategy("tenant-a", &m, "plan").unwrap();
        spine.approve_strategy("tenant-a", &m).unwrap();
        let arg = format!("{m}|grow|planner");
        // Plan as tenant A (operator).
        let plan_ctx = fake_ctx_tenant(arg.as_bytes(), "tenant-a");
        assert!(matches!(
            handle_team_plan(&agents, &spine, &plan_ctx),
            HandlerOutcome::Ok(_)
        ));
        // Tenant A reads its plan.
        let a = ok_body(handle_team_plan_latest(
            &spine,
            &fake_ctx_tenant(m.as_bytes(), "tenant-a"),
        ));
        assert_ne!(a.trim(), "null", "tenant A must see its own plan");
        // Tenant B reads null (cannot see tenant A's plan).
        let b = ok_body(handle_team_plan_latest(
            &spine,
            &fake_ctx_tenant(m.as_bytes(), "tenant-b"),
        ));
        assert_eq!(b.trim(), "null", "tenant B must not read tenant A's plan");
    }

    // ── Spawn-Key enforcement (company-model §5.2A) ──────────

    /// The hex subject_id a `fake_ctx_with_role(_, _, seed)` actor
    /// carries, so a test profile can be keyed to that caller.
    fn subject_of(seed: &[u8]) -> String {
        relix_core::types::NodeId::from_pubkey(seed).to_string()
    }

    #[test]
    fn agent_actor_without_spawn_key_is_denied() {
        let s = store();
        // Actor exists but is default-deny on can_spawn_agents.
        s.create_agent(
            "Planner",
            "planner",
            "Lead planner",
            "ops",
            "ops",
            "prime",
            &subject_of(b"planner-seed"),
            "medium",
            "default",
        )
        .unwrap();
        let arg = b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium";
        let out = handle_request_hire(&s, &fake_ctx_with_role(arg, "planner", b"planner-seed"));
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
    }

    #[test]
    fn agent_actor_with_direct_spawn_key_mints_pending_hire() {
        let s = store();
        let actor = s
            .create_agent(
                "Planner",
                "planner",
                "Lead",
                "ops",
                "ops",
                "prime",
                &subject_of(b"planner-seed"),
                "medium",
                "default",
            )
            .unwrap();
        s.update_agent_field(&actor, "can_spawn_agents", "true")
            .unwrap();
        s.update_agent_field(&actor, "spawn_route", "direct")
            .unwrap();
        let arg = b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium";
        let body = ok_body(handle_request_hire(
            &s,
            &fake_ctx_with_role(arg, "planner", b"planner-seed"),
        ));
        // direct route: no escalation note, and the hire is pending-inert.
        assert!(!body.contains("clearance:"), "{body}");
        let id = body.lines().next().unwrap().trim();
        assert_eq!(s.get_agent(id).unwrap().unwrap().status, "pending");
    }

    #[test]
    fn agent_actor_with_founder_route_gets_clearance_note() {
        let s = store();
        let actor = s
            .create_agent(
                "Planner",
                "planner",
                "Lead",
                "ops",
                "ops",
                "prime",
                &subject_of(b"planner-seed"),
                "medium",
                "default",
            )
            .unwrap();
        // can_spawn on, spawn_route stays the default ('founder').
        s.update_agent_field(&actor, "can_spawn_agents", "true")
            .unwrap();
        let arg = b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium";
        let body = ok_body(handle_request_hire(
            &s,
            &fake_ctx_with_role(arg, "planner", b"planner-seed"),
        ));
        assert!(
            body.contains("clearance:"),
            "founder route must surface a clearance note: {body}"
        );
    }

    // ── Route-differentiated spawn Clearance (company-model §5.2A) ──

    /// Build a `can_spawn_agents` actor with the given route, keyed to
    /// the `planner-seed` ctx, and return its agent_id.
    fn spawn_actor(s: &AgentStore, route: &str) -> String {
        let actor = s
            .create_agent(
                "Planner",
                "planner",
                "Lead",
                "ops",
                "ops",
                "prime",
                &subject_of(b"planner-seed"),
                "medium",
                "default",
            )
            .unwrap();
        s.update_agent_field(&actor, "can_spawn_agents", "true")
            .unwrap();
        s.update_agent_field(&actor, "spawn_route", route).unwrap();
        actor
    }

    /// Find the pending spawn Clearance minted for `hire_id`, if any.
    fn spawn_clearance_for(s: &AgentStore, hire_id: &str) -> Option<String> {
        use crate::nodes::coordinator::agent::store::SPAWN_CLEARANCE_METHOD;
        s.list_pending_approvals(100)
            .unwrap()
            .into_iter()
            .find(|r| r.method == SPAWN_CLEARANCE_METHOD && r.agent_id == hire_id)
            .map(|r| r.approval_id)
    }

    fn decide(s: &AgentStore, approval_id: &str, decision: &str) -> HandlerOutcome {
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{approval_id}|{decision}|operator|ok");
        handle_approval_decide(
            s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            None,
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        )
    }

    #[test]
    fn spawn_route_founder_creates_pending_hire_and_pending_clearance() {
        let s = store();
        spawn_actor(&s, "founder");
        let body = ok_body(handle_request_hire(
            &s,
            &fake_ctx_with_role(
                b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium",
                "planner",
                b"planner-seed",
            ),
        ));
        let hire_id = body.lines().next().unwrap().trim();
        // The hire is pending-inert ...
        assert_eq!(s.get_agent(hire_id).unwrap().unwrap().status, "pending");
        // ... and a real typed spawn Clearance exists for it.
        assert!(
            spawn_clearance_for(&s, hire_id).is_some(),
            "founder route must mint a typed spawn Clearance: {body}"
        );
    }

    #[test]
    fn approving_spawn_clearance_activates_the_hire() {
        let s = store();
        spawn_actor(&s, "founder");
        let body = ok_body(handle_request_hire(
            &s,
            &fake_ctx_with_role(
                b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium",
                "planner",
                b"planner-seed",
            ),
        ));
        let hire_id = body.lines().next().unwrap().trim().to_string();
        let cid = spawn_clearance_for(&s, &hire_id).expect("clearance exists");
        // Still pending before the decision.
        assert_eq!(s.get_agent(&hire_id).unwrap().unwrap().status, "pending");
        // Approving the Clearance activates the hire.
        assert!(matches!(
            decide(&s, &cid, "approved"),
            HandlerOutcome::Ok(_)
        ));
        assert_eq!(s.get_agent(&hire_id).unwrap().unwrap().status, "active");
    }

    #[test]
    fn rejecting_spawn_clearance_does_not_activate_the_hire() {
        let s = store();
        spawn_actor(&s, "founder");
        let body = ok_body(handle_request_hire(
            &s,
            &fake_ctx_with_role(
                b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium",
                "planner",
                b"planner-seed",
            ),
        ));
        let hire_id = body.lines().next().unwrap().trim().to_string();
        let cid = spawn_clearance_for(&s, &hire_id).expect("clearance exists");
        assert!(matches!(
            decide(&s, &cid, "rejected"),
            HandlerOutcome::Ok(_)
        ));
        // Never activated (the existing hire flow disables a rejected hire).
        assert_ne!(s.get_agent(&hire_id).unwrap().unwrap().status, "active");
    }

    #[test]
    fn deciding_an_already_decided_clearance_errors_and_does_not_double_apply() {
        let s = store();
        spawn_actor(&s, "founder");
        let body = ok_body(handle_request_hire(
            &s,
            &fake_ctx_with_role(
                b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium",
                "planner",
                b"planner-seed",
            ),
        ));
        let hire_id = body.lines().next().unwrap().trim().to_string();
        let cid = spawn_clearance_for(&s, &hire_id).expect("clearance exists");
        // First approve activates the hire exactly once.
        assert!(matches!(
            decide(&s, &cid, "approved"),
            HandlerOutcome::Ok(_)
        ));
        assert_eq!(s.get_agent(&hire_id).unwrap().unwrap().status, "active");
        // Manually flip it to a sentinel so a (forbidden) re-activation
        // would be observable.
        s.update_agent_field(&hire_id, "status", "suspended")
            .unwrap();
        // Second decide is refused (already terminal) — no double apply.
        let out = decide(&s, &cid, "approved");
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
        assert_eq!(
            s.get_agent(&hire_id).unwrap().unwrap().status,
            "suspended",
            "a re-decide must not re-activate the hire"
        );
        // Rejecting it afterwards is likewise refused.
        let out = decide(&s, &cid, "rejected");
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn clearance_cannot_be_decided_cross_tenant() {
        // An approval minted in tenant A is invisible (not-found) to a
        // caller scoped to tenant B — it can be neither read nor decided.
        let s = store();
        let id = s
            .create_approval(
                "agt_x",
                "subj",
                "m",
                "c",
                "",
                "needs yes",
                &[],
                None,
                9_999_999_999,
                &[],
                "tenant-a",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|operator|ok");
        let out = handle_approval_decide(
            &s,
            &fake_ctx_tenant(arg.as_bytes(), "tenant-b"),
            &resume,
            &fail,
            None,
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        );
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
        // The approval is still pending (untouched by the cross-tenant try).
        assert_eq!(
            s.get_approval(&id).unwrap().unwrap().status.as_wire(),
            "pending"
        );
        // Tenant A can still decide it.
        let arg = format!("{id}|approved|operator|ok");
        let out = handle_approval_decide(
            &s,
            &fake_ctx_tenant(arg.as_bytes(), "tenant-a"),
            &resume,
            &fail,
            None,
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        );
        assert!(matches!(out, HandlerOutcome::Ok(_)));
    }

    #[test]
    fn direct_route_mints_no_spawn_clearance() {
        let s = store();
        spawn_actor(&s, "direct");
        let body = ok_body(handle_request_hire(
            &s,
            &fake_ctx_with_role(
                b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium",
                "planner",
                b"planner-seed",
            ),
        ));
        let hire_id = body.lines().next().unwrap().trim();
        assert!(
            spawn_clearance_for(&s, hire_id).is_none(),
            "direct route must NOT mint a spawn Clearance"
        );
    }

    #[test]
    fn denied_spawn_creates_neither_hire_nor_clearance() {
        let s = store();
        // can_spawn defaults false → denied.
        s.create_agent(
            "Planner",
            "planner",
            "Lead",
            "ops",
            "ops",
            "prime",
            &subject_of(b"planner-seed"),
            "medium",
            "default",
        )
        .unwrap();
        let before = s.list_pending_approvals(100).unwrap().len();
        let out = handle_request_hire(
            &s,
            &fake_ctx_with_role(
                b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium",
                "planner",
                b"planner-seed",
            ),
        );
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
        // No new pending approval, and no Operative named "Worker".
        assert_eq!(s.list_pending_approvals(100).unwrap().len(), before);
        assert!(
            !s.list_agents(None)
                .unwrap()
                .iter()
                .any(|a| a.name == "Worker"),
            "a denied spawn must mint no hire"
        );
    }

    #[test]
    fn agent_create_is_operator_only() {
        let s = store();
        let arg = b"Worker|engineer|Worker|eng|eng|planner|subj-worker|medium";
        // An agent actor cannot conjure a live Operative via agent.create.
        let out = handle_create(&s, &fake_ctx_with_role(arg, "planner", b"planner-seed"));
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
        // The Founder/Board path still works.
        assert!(matches!(
            handle_create(&s, &fake_ctx(arg)),
            HandlerOutcome::Ok(_)
        ));
    }

    #[test]
    fn unknown_actor_spawn_is_security_denied() {
        let s = store();
        // Non-operator role with no Operative profile for the subject.
        let out = handle_request_hire(
            &s,
            &fake_ctx_with_role(
                b"W|engineer|W|e|e|p|subj-w|medium",
                "planner",
                b"ghost-seed",
            ),
        );
        assert_eq!(err_kind(out), error_kinds::SECURITY_DENIED);
    }

    // ── Configure-Key enforcement (company-model §5.2A) ──────

    /// Create a configurer actor keyed to the `cfg-seed` ctx with the
    /// given scope, plus a report (in its Branch) and an out-of-Branch
    /// outsider. Returns (store, actor_id, report_id, outsider_id).
    fn configure_fixture(scope: &str) -> (AgentStore, String, String, String) {
        let s = store();
        let actor = s
            .create_agent(
                "Lead",
                "planner",
                "L",
                "ops",
                "ops",
                "prime",
                &subject_of(b"cfg-seed"),
                "medium",
                "default",
            )
            .unwrap();
        s.update_agent_field(&actor, "can_configure_agents", "true")
            .unwrap();
        s.update_agent_field(&actor, "configure_scope", scope)
            .unwrap();
        let report = s
            .create_agent(
                "R", "engineer", "R", "e", "e", "l", "subj-r", "medium", "default",
            )
            .unwrap();
        s.update_agent_field(&report, "reports_to", &actor).unwrap();
        let outsider = s
            .create_agent(
                "O", "engineer", "O", "e", "e", "x", "subj-o", "medium", "default",
            )
            .unwrap();
        (s, actor, report, outsider)
    }

    fn update_as(s: &AgentStore, target: &str, field: &str, value: &str) -> HandlerOutcome {
        handle_update(
            s,
            &fake_ctx_with_role(
                format!("{target}|{field}|{value}").as_bytes(),
                "planner",
                b"cfg-seed",
            ),
        )
    }

    #[test]
    fn configure_non_configurer_cannot_update_another_agent() {
        let s = store();
        // Actor exists but lacks can_configure_agents.
        s.create_agent(
            "Lead",
            "planner",
            "L",
            "ops",
            "ops",
            "prime",
            &subject_of(b"cfg-seed"),
            "medium",
            "default",
        )
        .unwrap();
        let target = s
            .create_agent(
                "T", "engineer", "T", "e", "e", "x", "subj-t", "medium", "default",
            )
            .unwrap();
        assert_eq!(
            err_kind(update_as(&s, &target, "title", "Hacked")),
            error_kinds::POLICY_DENIED
        );
    }

    #[test]
    fn configure_branch_scope_updates_report_not_peer() {
        let (s, _actor, report, outsider) = configure_fixture("branch");
        assert!(matches!(
            update_as(&s, &report, "title", "Senior"),
            HandlerOutcome::Ok(_)
        ));
        assert_eq!(s.get_agent(&report).unwrap().unwrap().title, "Senior");
        assert_eq!(
            err_kind(update_as(&s, &outsider, "title", "X")),
            error_kinds::POLICY_DENIED
        );
    }

    #[test]
    fn configure_specific_scope_honours_allowlist() {
        let (s, actor, _report, outsider) = configure_fixture("specific");
        let listed = s
            .create_agent(
                "Lstd", "engineer", "L", "e", "e", "x", "subj-ls", "medium", "default",
            )
            .unwrap();
        s.update_agent_field(&actor, "configure_allowed_agents", &listed)
            .unwrap();
        assert!(matches!(
            update_as(&s, &listed, "title", "Y"),
            HandlerOutcome::Ok(_)
        ));
        assert_eq!(
            err_kind(update_as(&s, &outsider, "title", "Z")),
            error_kinds::POLICY_DENIED
        );
    }

    #[test]
    fn configure_self_escalation_is_denied() {
        // An actor with full configure rights still cannot edit itself.
        let (s, actor, _r, _o) = configure_fixture("any");
        assert_eq!(
            err_kind(update_as(&s, &actor, "can_spawn_agents", "true")),
            error_kinds::POLICY_DENIED
        );
        // ... and certainly cannot disable itself.
        let out = handle_delete(
            &s,
            &fake_ctx_with_role(actor.as_bytes(), "planner", b"cfg-seed"),
        );
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
    }

    #[test]
    fn configure_wrong_tenant_target_is_denied() {
        let (s, actor, _r, _o) = configure_fixture("any");
        let _ = actor;
        let other = s
            .create_agent(
                "Ot", "engineer", "O", "e", "e", "x", "subj-ot", "medium", "tenant-b",
            )
            .unwrap();
        assert_eq!(
            err_kind(update_as(&s, &other, "title", "X")),
            error_kinds::POLICY_DENIED
        );
    }

    #[test]
    fn configure_founder_path_is_preserved() {
        let (s, _a, report, outsider) = configure_fixture("none");
        // Operator (Founder) bypasses regardless of scope=none.
        assert!(matches!(
            handle_update(&s, &fake_ctx(format!("{report}|title|F").as_bytes())),
            HandlerOutcome::Ok(_)
        ));
        assert!(matches!(
            handle_update(&s, &fake_ctx(format!("{outsider}|title|F").as_bytes())),
            HandlerOutcome::Ok(_)
        ));
    }

    // ── Assign-Key verdict (company-model §5.2B / §5.3) ──────

    #[test]
    fn assign_check_branch_scope_allows_in_branch_denies_out() {
        let s = store();
        let mgr = s
            .create_agent(
                "Mgr", "planner", "Lead", "ops", "ops", "prime", "subj-mgr", "medium", "default",
            )
            .unwrap();
        s.update_agent_field(&mgr, "can_assign_work", "true")
            .unwrap();
        s.update_agent_field(&mgr, "assign_scope", "branch")
            .unwrap();
        let worker = s
            .create_agent(
                "W", "engineer", "W", "eng", "eng", "mgr", "subj-w", "medium", "default",
            )
            .unwrap();
        s.update_agent_field(&worker, "reports_to", &mgr).unwrap();
        let outsider = s
            .create_agent(
                "O", "engineer", "O", "eng", "eng", "x", "subj-o", "medium", "default",
            )
            .unwrap();
        let body = ok_body(handle_assign_check(
            &s,
            &fake_ctx(format!("{mgr}|{worker}").as_bytes()),
        ));
        assert!(body.contains("\"allow\""), "in-branch should allow: {body}");
        let body = ok_body(handle_assign_check(
            &s,
            &fake_ctx(format!("{mgr}|{outsider}").as_bytes()),
        ));
        assert!(
            body.contains("\"deny\""),
            "out-of-branch should deny: {body}"
        );
    }

    #[test]
    fn assign_check_denies_without_key() {
        let s = store();
        let mgr = s
            .create_agent(
                "Mgr", "planner", "Lead", "ops", "ops", "prime", "subj-mgr", "medium", "default",
            )
            .unwrap();
        let worker = s
            .create_agent(
                "W", "engineer", "W", "eng", "eng", "mgr", "subj-w", "medium", "default",
            )
            .unwrap();
        // can_assign_work defaults false (default-deny).
        let body = ok_body(handle_assign_check(
            &s,
            &fake_ctx(format!("{mgr}|{worker}").as_bytes()),
        ));
        assert!(body.contains("\"deny\""), "{body}");
    }

    #[test]
    fn assign_check_is_tenant_scoped() {
        // GROUP 6: agent.assign_check resolves the actor by agent_id
        // scoped to the caller's tenant — tenant B cannot probe tenant
        // A's Operative.
        let s = store();
        let mgr = s
            .create_agent(
                "Mgr", "planner", "Lead", "ops", "ops", "prime", "subj-mgr", "medium", "tenant-a",
            )
            .unwrap();
        let worker = s
            .create_agent(
                "W", "engineer", "W", "eng", "eng", "mgr", "subj-w", "medium", "tenant-a",
            )
            .unwrap();
        s.update_agent_field_for_tenant(&mgr, "tenant-a", "can_assign_work", "true")
            .unwrap();
        s.update_agent_field_for_tenant(&mgr, "tenant-a", "assign_scope", "any")
            .unwrap();
        // From tenant A the verdict resolves (allow).
        let body = ok_body(handle_assign_check(
            &s,
            &fake_ctx_tenant(format!("{mgr}|{worker}").as_bytes(), "tenant-a"),
        ));
        assert!(body.contains("\"allow\""), "{body}");
        // From tenant B the actor is not found — never a cross-tenant read.
        let out = handle_assign_check(
            &s,
            &fake_ctx_tenant(format!("{mgr}|{worker}").as_bytes(), "tenant-b"),
        );
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn brief_clearance_request_requires_assigned_active_agent() {
        let agents = store();
        let tasks = TaskStore::in_memory().unwrap();
        let agent = agents
            .create_agent(
                "Worker",
                "engineer",
                "Worker",
                "eng",
                "eng",
                "prime",
                "subj-worker",
                "medium",
                "default",
            )
            .unwrap();
        let brief = tasks
            .create(
                "Risky work",
                "flow.sol",
                "{}",
                "owner",
                crate::nodes::coordinator::RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        let arg = format!("{brief}|{agent}|tool.terminal|terminal|need shell access|300");

        let out = handle_brief_clearance_request(&agents, &tasks, &fake_ctx(arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::POLICY_DENIED);
    }

    #[test]
    fn brief_clearance_request_creates_pending_approval_and_parks_brief() {
        let agents = store();
        let tasks = TaskStore::in_memory().unwrap();
        let agent = agents
            .create_agent(
                "Worker",
                "engineer",
                "Worker",
                "eng",
                "eng",
                "prime",
                "subj-worker",
                "medium",
                "default",
            )
            .unwrap();
        let brief = tasks
            .create(
                "Risky work",
                "flow.sol",
                "{}",
                "owner",
                crate::nodes::coordinator::RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        tasks.set_brief_field(&brief, "assignee", &agent).unwrap();
        let arg = format!("{brief}|{agent}|tool.terminal|terminal|need shell access|300");

        let approval_id = ok_body(handle_brief_clearance_request(
            &agents,
            &tasks,
            &fake_ctx(arg.as_bytes()),
        ))
        .trim()
        .to_string();
        let approval = agents.get_approval(&approval_id).unwrap().unwrap();
        assert_eq!(approval.status, ApprovalStatus::Pending);
        assert_eq!(approval.agent_id, agent);
        assert_eq!(approval.subject_id, "subj-worker");
        assert_eq!(approval.method, "tool.terminal");
        assert_eq!(approval.task_id.as_deref(), Some(brief.as_str()));
        assert_eq!(tasks.get(&brief).unwrap().unwrap().status, "awaiting_input");
        let events = tasks
            .query_events(
                &brief,
                0,
                20,
                Some("brief.clearance_requested"),
                crate::nodes::coordinator::EventOrder::Asc,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].payload.contains(&approval_id));
    }

    #[test]
    fn get_handler_returns_all_fields() {
        let s = store();
        let id = s
            .create_agent(
                "Research", "research", "Junior", "rd", "ops", "alice", "subj-1", "medium",
                "default",
            )
            .unwrap();
        let body = ok_body(handle_get(&s, &fake_ctx(id.as_bytes())));
        for needle in [
            "agent_id=",
            "name=Research",
            "role=research",
            "status=active",
            "risk_ceiling=medium",
            "subject_id=subj-1",
            "approval_required_categories=",
        ] {
            assert!(body.contains(needle), "missing {needle:?}: {body}");
        }
    }

    #[test]
    fn list_handler_filters_by_subject() {
        let s = store();
        s.create_agent("a", "r", "t", "d", "t", "c", "subj-1", "low", "default")
            .unwrap();
        s.create_agent("b", "r", "t", "d", "t", "c", "subj-2", "low", "default")
            .unwrap();
        let body = ok_body(handle_list(&s, &fake_ctx(b"subj-1")));
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "count=1");
    }

    #[test]
    fn update_handler_toggles_status() {
        let s = store();
        let id = s
            .create_agent("a", "r", "t", "d", "t", "c", "subj", "medium", "default")
            .unwrap();
        let arg = format!("{id}|status|suspended");
        let out = handle_update(&s, &fake_ctx(arg.as_bytes()));
        assert_eq!(ok_body(out), "ok\n");
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "suspended");
    }

    #[test]
    fn delete_handler_disables_the_row() {
        let s = store();
        let id = s
            .create_agent("a", "r", "t", "d", "t", "c", "subj", "medium", "default")
            .unwrap();
        let out = handle_delete(&s, &fake_ctx(id.as_bytes()));
        assert_eq!(ok_body(out), "ok\n");
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "disabled");
    }

    #[test]
    fn effective_capabilities_intersects_with_allow_categories() {
        let s = store();
        let id = s
            .create_agent("a", "r", "t", "d", "t", "c", "subj", "medium", "default")
            .unwrap();
        s.update_agent_field(&id, "allow_categories", "browser, fetch")
            .unwrap();
        let arg = format!("{id}|ai");
        let out = handle_effective_capabilities(&s, &fake_ctx(arg.as_bytes()), |_| {
            vec![
                (
                    "tool.browser.click".into(),
                    vec!["browser".into()],
                    vec![],
                    "medium".into(),
                ),
                (
                    "tool.web_fetch".into(),
                    vec!["fetch".into()],
                    vec![],
                    "low".into(),
                ),
                (
                    "payments.charge".into(),
                    vec!["payments".into()],
                    vec![],
                    "high".into(),
                ),
            ]
        });
        let body = ok_body(out);
        assert!(body.contains("tool.browser.click"));
        assert!(body.contains("tool.web_fetch"));
        assert!(!body.contains("payments.charge"));
        assert!(body.contains("count=2"));
    }

    #[test]
    fn effective_capabilities_returns_zero_for_disabled_agent() {
        let s = store();
        let id = s
            .create_agent("a", "r", "t", "d", "t", "c", "subj", "medium", "default")
            .unwrap();
        s.soft_delete_agent(&id).unwrap();
        let arg = format!("{id}|ai");
        let out = handle_effective_capabilities(&s, &fake_ctx(arg.as_bytes()), |_| Vec::new());
        let body = ok_body(out);
        assert!(body.contains("count=0"));
        assert!(body.contains("reason=agent_disabled"));
    }

    #[test]
    fn risk_within_ceiling_table() {
        assert!(risk_within_ceiling("low", "medium"));
        assert!(risk_within_ceiling("medium", "medium"));
        assert!(!risk_within_ceiling("high", "medium"));
        assert!(risk_within_ceiling("critical", "critical"));
        assert!(!risk_within_ceiling("garbage", "high"));
    }

    #[test]
    fn approval_pending_returns_correct_row_count() {
        let s = store();
        s.create_approval(
            "a",
            "s",
            "m",
            "c",
            "",
            "r1",
            &[],
            None,
            9999999999,
            &[],
            "default",
        )
        .unwrap();
        s.create_approval(
            "a",
            "s",
            "m",
            "c",
            "",
            "r2",
            &[],
            None,
            9999999999,
            &[],
            "default",
        )
        .unwrap();
        let body = ok_body(handle_approval_pending(&s, &fake_ctx(b"")));
        assert!(body.contains("count=2"));
    }

    fn test_signer() -> crate::approval::ApprovalSigner {
        crate::approval::ApprovalSigner::from_seed([9u8; 32])
    }

    fn test_keyset() -> crate::approval::ApprovalKeySet {
        crate::approval::ApprovalKeySet::from_signer(&test_signer())
    }

    #[test]
    fn approval_decide_approves_and_mints_structured_token() {
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9999999999,
                &[],
                "default",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|alice|ok");
        let body = ok_body(handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        ));
        assert!(body.starts_with("ok|"));
        let wire = body.trim_start_matches("ok|").trim();
        // SEC PART A: the wire token must parse + verify with
        // the same key the handler signed it with.
        let tok = crate::approval::ApprovalToken::parse(wire).unwrap();
        tok.verify_signature(&test_keyset())
            .expect("token signature must verify");
        assert_eq!(tok.approval_id, id);
        assert_eq!(tok.method, "m");
        assert_eq!(tok.subject_id, "s");
    }

    #[test]
    fn approval_decide_approves_without_key_omits_token() {
        // P1 fail-loud path: no signer ⇒ returns `ok\n` so
        // operators noticing missing tokens reach the controller
        // boot log warning.
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9999999999,
                &[],
                "default",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|alice|ok");
        let body = ok_body(handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            None,
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        ));
        assert_eq!(body, "ok\n");
    }

    #[test]
    fn approval_decide_rejects_returns_ok_without_token() {
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9999999999,
                &[],
                "default",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|rejected|alice|nope");
        let body = ok_body(handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        ));
        assert_eq!(body, "ok\n");
    }

    // ── task_id round-trip on the approval row ───────────

    #[test]
    fn approval_decide_invokes_resume_closure_with_stored_task_id() {
        let s = store();
        // Approval stamped with task_id = "task-42".
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                Some("task-42"),
                9999999999,
                &[],
                "default",
            )
            .unwrap();
        let resumed: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let failed: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let resumed_clone = resumed.clone();
        let resume: TaskResumeFn = Arc::new(move |tid: &str| {
            *resumed_clone.lock().unwrap() = Some(tid.to_string());
            Ok(())
        });
        let failed_clone = failed.clone();
        let fail: TaskResumeFn = Arc::new(move |tid: &str| {
            *failed_clone.lock().unwrap() = Some(tid.to_string());
            Ok(())
        });
        let arg = format!("{id}|approved|alice|ok");
        let _ = handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        );
        assert_eq!(resumed.lock().unwrap().as_deref(), Some("task-42"));
        assert!(failed.lock().unwrap().is_none());
    }

    #[test]
    fn approval_decide_invokes_fail_closure_for_reject_with_stored_task_id() {
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                Some("task-99"),
                9999999999,
                &[],
                "default",
            )
            .unwrap();
        let failed: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let failed_clone = failed.clone();
        let fail: TaskResumeFn = Arc::new(move |tid: &str| {
            *failed_clone.lock().unwrap() = Some(tid.to_string());
            Ok(())
        });
        let arg = format!("{id}|rejected|alice|nope");
        let _ = handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        );
        assert_eq!(failed.lock().unwrap().as_deref(), Some("task-99"));
    }

    #[test]
    fn approval_decide_skips_task_hop_when_row_has_no_task_id() {
        // Backward-compat: approval rows minted without a
        // task_id (older flows that didn't thread one through
        // the envelope) still decide cleanly. The
        // resume / fail closures are never called.
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9999999999,
                &[],
                "default",
            )
            .unwrap();
        let count: Arc<std::sync::atomic::AtomicUsize> =
            Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resume_count = count.clone();
        let fail_count = count.clone();
        let resume: TaskResumeFn = Arc::new(move |_| {
            resume_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let fail: TaskResumeFn = Arc::new(move |_| {
            fail_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let arg = format!("{id}|approved|alice|ok");
        let body = ok_body(handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        ));
        // Cleanly approves (returns the one-shot signed
        // token) but never invokes either closure.
        assert!(body.starts_with("ok|"));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // ── DEFERRED 1: configurable token TTL ──────────────

    #[test]
    fn clamp_token_ttl_returns_default_when_unset() {
        assert_eq!(
            clamp_approval_token_ttl_secs(None),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS
        );
    }

    #[test]
    fn clamp_token_ttl_clamps_below_min_to_30s() {
        assert_eq!(
            clamp_approval_token_ttl_secs(Some(0)),
            APPROVAL_TOKEN_TTL_MIN_SECS
        );
        assert_eq!(
            clamp_approval_token_ttl_secs(Some(1)),
            APPROVAL_TOKEN_TTL_MIN_SECS
        );
        assert_eq!(
            clamp_approval_token_ttl_secs(Some(29)),
            APPROVAL_TOKEN_TTL_MIN_SECS
        );
    }

    #[test]
    fn clamp_token_ttl_clamps_above_max_to_86400s() {
        assert_eq!(
            clamp_approval_token_ttl_secs(Some(86_401)),
            APPROVAL_TOKEN_TTL_MAX_SECS
        );
        assert_eq!(
            clamp_approval_token_ttl_secs(Some(u64::MAX)),
            APPROVAL_TOKEN_TTL_MAX_SECS
        );
    }

    #[test]
    fn clamp_token_ttl_passes_value_through_when_in_range() {
        assert_eq!(clamp_approval_token_ttl_secs(Some(30)), 30);
        assert_eq!(clamp_approval_token_ttl_secs(Some(60)), 60);
        assert_eq!(clamp_approval_token_ttl_secs(Some(3600)), 3600);
        assert_eq!(clamp_approval_token_ttl_secs(Some(86_400)), 86_400);
    }

    #[test]
    fn approval_decide_with_60s_ttl_mints_token_with_60s_expiry() {
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9999999999,
                &[],
                "default",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|alice|ok");
        let body = ok_body(handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            Some(&test_signer()),
            60,
            &relix_core::clock::SystemClock,
        ));
        let wire = body.trim_start_matches("ok|").trim();
        let tok = crate::approval::ApprovalToken::parse(wire).unwrap();
        // Token must expire within ~60s of issue. The handler
        // uses wall-clock now() for `issued_at_ms`, so we
        // verify the delta is the requested TTL converted to
        // milliseconds.
        let delta_ms = tok.expires_at_ms - tok.issued_at_ms;
        assert_eq!(
            delta_ms, 60_000,
            "60s TTL must mint a 60_000ms-window token"
        );
    }

    #[test]
    fn approval_decide_with_3600s_ttl_mints_long_lived_token() {
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9999999999,
                &[],
                "default",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|alice|ok");
        let body = ok_body(handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
            Some(&test_signer()),
            3_600,
            &relix_core::clock::SystemClock,
        ));
        let wire = body.trim_start_matches("ok|").trim();
        let tok = crate::approval::ApprovalToken::parse(wire).unwrap();
        let delta_ms = tok.expires_at_ms - tok.issued_at_ms;
        assert_eq!(delta_ms, 3_600_000, "3600s TTL must mint a 1h-window token");
        // And the token IS valid 60s after issue (the
        // structural check; the gate's time check would also
        // pass at any time in the window).
        tok.check_not_expired(tok.issued_at_ms + 60_000)
            .expect("3600s token is still valid at issued+60s");
    }

    // ── DEFERRED 3: legacy-token migration agent-side signal ──

    #[test]
    fn approval_get_returns_pending_status_for_fresh_row() {
        // DEFERRED C: the wire response is now JSON. Verify the
        // shape carries every documented field.
        let s = store();
        let id = s
            .create_approval(
                "a",
                "subj-1",
                "tool.web_read",
                "external_api:read",
                "",
                "fetch user",
                &[],
                None,
                9_999_999_999,
                &["subj-op".into()],
                "default",
            )
            .unwrap();
        let body = ok_body(handle_approval_get(&s, &fake_ctx(id.as_bytes())));
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
        assert_eq!(v["status"], "pending");
        assert_eq!(v["approval_id"], id);
        assert_eq!(v["agent_id"], "a");
        assert_eq!(v["subject_id"], "subj-1");
        assert_eq!(v["method"], "tool.web_read");
        assert_eq!(v["capability_category"], "external_api:read");
        assert_eq!(v["reason"], "fetch user");
        assert!(v["decided_at"].is_null());
        assert!(v["decided_by"].is_null());
        assert!(v["decision_note"].is_null());
        assert!(v["task_id"].is_null());
        assert_eq!(v["authorized_approvers"], serde_json::json!(["subj-op"]));
    }

    #[test]
    fn approval_get_surfaces_legacy_token_expired_for_migrated_row() {
        // DEFERRED 3 + DEFERRED C: an agent polling
        // `coord.approval.get` on a migrated approval sees the
        // `legacy_token_expired` status + the explanatory
        // decision note in the JSON body.
        let s = store();
        s.seed_legacy_token_row_for_test("leg-poll", "pending", "deadbeef")
            .unwrap();
        let n = s.run_legacy_token_migration_for_test().unwrap();
        assert_eq!(n, 1, "the seeded row must be migrated");
        let body = ok_body(handle_approval_get(&s, &fake_ctx(b"leg-poll")));
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
        assert_eq!(v["status"], "legacy_token_expired");
        assert!(
            v["decision_note"]
                .as_str()
                .unwrap_or("")
                .contains("legacy_token_expired:"),
            "decision note must explain the migration: {v}"
        );
    }

    #[test]
    fn approval_get_returns_invalid_args_for_unknown_id() {
        let s = store();
        match handle_approval_get(&s, &fake_ctx(b"nope")) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("not found"));
            }
            HandlerOutcome::Ok(_) => panic!("expected INVALID_ARGS"),
        }
    }

    // ── DEFERRED 2: authorised-approver check on coord.approval.decide ──

    #[test]
    fn approval_decide_denies_non_operator_when_not_in_authorized_approvers() {
        // SEC PART B / DEFERRED 2: an `agent`-role caller that
        // is NOT in the row's `authorized_approvers` cannot
        // decide. Mirrors the §7.30 `handle_record_decision`
        // contract for the AgentStore-backed path.
        let s = store();
        let approver_subject = relix_core::types::NodeId::from_pubkey(b"operator-bob").to_string();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9_999_999_999,
                std::slice::from_ref(&approver_subject),
                "default",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|alice|");
        let ctx = fake_ctx_with_role(arg.as_bytes(), "agent", b"random-agent");
        let out = handle_approval_decide(
            &s,
            &ctx,
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        );
        match out {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::SECURITY_DENIED);
                assert!(
                    env.cause.contains("not an authorised approver"),
                    "got cause: {}",
                    env.cause
                );
            }
            HandlerOutcome::Ok(_) => panic!("unauthorised approval must NOT admit"),
        }
        // Row stays pending.
        let r = s.get_approval(&id).unwrap().unwrap();
        assert_eq!(r.status, ApprovalStatus::Pending);
    }

    #[test]
    fn approval_decide_admits_listed_subject_with_non_operator_role() {
        // Subject is in `authorized_approvers` → admission
        // succeeds even when role is just `agent`.
        let s = store();
        let approver_subject = relix_core::types::NodeId::from_pubkey(b"operator-bob").to_string();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9_999_999_999,
                &[approver_subject],
                "default",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|alice|");
        let ctx = fake_ctx_with_role(arg.as_bytes(), "agent", b"operator-bob");
        let out = handle_approval_decide(
            &s,
            &ctx,
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        );
        assert!(matches!(out, HandlerOutcome::Ok(_)));
        let r = s.get_approval(&id).unwrap().unwrap();
        assert_eq!(r.status, ApprovalStatus::Approved);
    }

    #[test]
    fn approval_decide_admits_operator_role_when_allow_list_empty() {
        // Empty allow-list ⇒ role-based fallback (operator /
        // admin only). This is the "no policy defines
        // authorized_approvers" default the user specified.
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9_999_999_999,
                // Empty allow-list explicitly.
                &[],
                "default",
            )
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|alice|");
        // Non-operator → denied even though the allow-list is
        // empty, proving the empty-list ≠ open-to-everyone
        // invariant.
        let ctx_agent = fake_ctx_with_role(arg.as_bytes(), "agent", b"random-agent");
        let out_deny = handle_approval_decide(
            &s,
            &ctx_agent,
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        );
        match out_deny {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::SECURITY_DENIED);
            }
            HandlerOutcome::Ok(_) => panic!("agent role + empty allow-list must NOT admit"),
        }
        // Operator → admits.
        let ctx_op = fake_ctx_with_role(arg.as_bytes(), "operator", b"oncall-1");
        let out_ok = handle_approval_decide(
            &s,
            &ctx_op,
            &resume,
            &fail,
            Some(&test_signer()),
            APPROVAL_TOKEN_TTL_DEFAULT_SECS,
            &relix_core::clock::SystemClock,
        );
        assert!(matches!(out_ok, HandlerOutcome::Ok(_)));
    }

    #[test]
    fn standing_create_then_list_then_revoke_round_trips() {
        let s = store();
        let arg = "agt-1|fs|9999999999|alice|monthly window";
        let id = ok_body(handle_standing_create(&s, &fake_ctx(arg.as_bytes())))
            .trim()
            .to_string();
        assert!(id.starts_with("std_"));
        let body = ok_body(handle_standing_list(&s, &fake_ctx(b"agt-1")));
        assert!(body.contains("count=1"));
        let body = ok_body(handle_standing_revoke(&s, &fake_ctx(id.as_bytes())));
        assert_eq!(body, "ok\n");
    }

    #[test]
    fn default_approval_required_categories_matches_spec() {
        let v = default_approval_required_categories();
        assert!(v.contains(&"payments".to_string()));
        assert!(v.contains(&"production_deploy".to_string()));
        assert!(v.contains(&"credentials:read".to_string()));
        assert!(v.contains(&"email:send".to_string()));
        assert!(v.contains(&"external_api:write".to_string()));
        assert!(v.contains(&"browser.form_submit".to_string()));
    }
}
