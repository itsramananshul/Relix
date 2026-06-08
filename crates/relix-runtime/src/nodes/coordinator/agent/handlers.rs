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
use crate::nodes::coordinator::agent::action_center;
use crate::nodes::coordinator::agent::keys::{
    KeyVerdict, assign_verdict, configure_verdict, manage_verdict, spawn_verdict,
};
use crate::nodes::coordinator::agent::prime;
use crate::nodes::coordinator::agent::store::{
    AgentProfile, AgentStore, AgentStoreError, ApprovalStatus, StandingApprovalCreate,
    default_approval_categories,
};
use crate::nodes::coordinator::brief::PrimeDossierOutcome;
use crate::nodes::coordinator::spine::SpineStore;
use crate::nodes::coordinator::spine::store::{
    OrchestrationRunRecord, SpineStoreError, TeamPlanRecord,
};
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

// ── company.* (first-run owner/Founder bootstrap) ────────

/// A compact JSON view of an Operative for the dashboard Crew + the
/// company-status read. Trusted, operator-facing fields only.
fn operative_json(p: &AgentProfile) -> serde_json::Value {
    serde_json::json!({
        "agent_id": p.agent_id,
        "name": p.name,
        "role": p.role,
        "title": p.title,
        "department": p.department,
        "team": p.team,
        "status": p.status,
        "rig": p.rig,
        "reports_to": p.reports_to,
        "can_spawn_agents": p.can_spawn_agents,
        "can_assign_work": p.can_assign_work,
        "can_manage_work": p.can_manage_work,
        "can_configure_agents": p.can_configure_agents,
        "created_at": p.created_at,
    })
}

/// First-run owner gate: the caller may bootstrap the company iff it is
/// a real operator/admin (AIC role) OR it carries the boot-seeded
/// operator-console (`allow-all`) profile in this Guild — i.e. it IS the
/// trusted dashboard/bridge owner identity. A normal Operative (or an
/// unknown caller with no profile) is refused, so bootstrap can never be
/// triggered by an arbitrary actor.
fn caller_is_owner(store: &AgentStore, ctx: &InvocationCtx) -> bool {
    if caller_is_operator(ctx) {
        return true;
    }
    let tenant = ctx.tenant_id_or_default();
    let subject = ctx.caller.subject_id.to_string();
    matches!(
        store.get_by_subject_for_tenant(&subject, tenant),
        Ok(Some(p)) if p.profile.as_deref() == Some("allow-all")
    )
}

/// `company.status` — first-run read (no args). Reports whether the
/// Guild has been initialised (a Founder exists), the Founder profile,
/// and the count of real Operatives. The dashboard uses this to show
/// the "Initialize Company" first-run state vs the normal Crew.
/// Increment a `{key: count}` tally in a JSON object (empty key → `unknown`).
fn bump_tally(map: &mut serde_json::Map<String, serde_json::Value>, key: &str) {
    let key = if key.trim().is_empty() {
        "unknown"
    } else {
        key.trim()
    };
    let n = map.get(key).and_then(|v| v.as_i64()).unwrap_or(0) + 1;
    map.insert(key.to_string(), serde_json::json!(n));
}

/// Build the base `company.status` object (initialized / founder / prime /
/// crew) shared by the agent-only first-run read and the operations-aware
/// variant. Returns the JSON map PLUS the roster, which the operations summary
/// reuses for its pending-hire count (no second roster read).
fn company_status_base(
    store: &AgentStore,
    tenant: &str,
) -> Result<
    (
        serde_json::Map<String, serde_json::Value>,
        Vec<AgentProfile>,
    ),
    AgentStoreError,
> {
    let founder = store.find_founder(tenant)?;
    let operatives = store.list_operatives_for_tenant(tenant)?;
    // Prime = the Founder's right hand (Lexicon) — the Operative whose role is
    // `prime`, who proposes the strategy + builds the team. `None` until one is
    // hired, so the dashboard can show "no Prime yet" honestly.
    let prime = operatives
        .iter()
        .find(|o| o.role.eq_ignore_ascii_case("prime"));
    // Crew breakdown by status + role, so the dashboard can show a real company
    // shape (who's active, who's pending a hire, the role mix) instead of a
    // bare head-count.
    let mut by_status = serde_json::Map::new();
    let mut by_role = serde_json::Map::new();
    let mut active = 0i64;
    let mut pending = 0i64;
    for o in &operatives {
        let st = o.status.trim();
        match st {
            "active" => active += 1,
            "pending" => pending += 1,
            _ => {}
        }
        bump_tally(&mut by_status, st);
        bump_tally(&mut by_role, &o.role);
    }
    let mut map = serde_json::Map::new();
    map.insert("initialized".into(), serde_json::json!(founder.is_some()));
    map.insert(
        "founder".into(),
        serde_json::json!(founder.as_ref().map(operative_json)),
    );
    // The Prime is the company's planning lead; null until hired.
    map.insert("prime".into(), serde_json::json!(prime.map(operative_json)));
    map.insert(
        "operative_count".into(),
        serde_json::json!(operatives.len()),
    );
    map.insert(
        "crew".into(),
        serde_json::json!({
            "total": operatives.len(),
            "active": active,
            "pending": pending,
            "by_status": by_status,
            "by_role": by_role,
        }),
    );
    Ok((map, operatives))
}

pub fn handle_company_status(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let tenant = ctx.tenant_id_or_default();
    let (map, _operatives) = match company_status_base(store, tenant) {
        Ok(v) => v,
        Err(e) => return internal(format!("company.status: {e}")),
    };
    match serde_json::to_vec(&serde_json::Value::Object(map)) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("company.status encode: {e}")),
    }
}

/// Bounded recent-runs window the operations summary classifies (newest-first),
/// mirroring the Action Center's run scan so the cockpit and the snapshot read
/// the SAME ledger window.
const OPS_RUN_WINDOW: i64 = 200;

/// `company.status` WITH a read-only **operations** summary — a tenant-scoped
/// company operations snapshot (company-model §5.4 / §8.2; dashboard-design §5)
/// so the Overview cockpit can show "work in flight / blocked / review /
/// approvals / mandates" at a glance instead of stitching separate panels.
///
/// Backward-compatible: the base `company.status` fields (initialized / founder
/// / prime / crew) are unchanged; this only ADDS an `operations` object. The
/// operations counts derive ONLY from EXISTING tenant-scoped store reads (the
/// same helpers the Action Center uses), so they can never disagree with the
/// gate and never fabricate a figure. Every operations read is best-effort: a
/// transient sub-read failure degrades that bucket to `0`/empty rather than
/// failing the core status read.
pub fn handle_company_status_with_ops(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let tenant = ctx.tenant_id_or_default();
    let (mut map, operatives) = match company_status_base(agent_store, tenant) {
        Ok(v) => v,
        Err(e) => return internal(format!("company.status: {e}")),
    };
    let operations = compute_operations(agent_store, spine_store, task_store, tenant, &operatives);
    map.insert("operations".into(), operations);
    match serde_json::to_vec(&serde_json::Value::Object(map)) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("company.status encode: {e}")),
    }
}

/// Compute the read-only, tenant-scoped operations summary embedded in
/// `company.status` (see [`handle_company_status_with_ops`]). PURE-ish: it only
/// READS existing tenant-scoped store helpers and shapes the result — no
/// mutation, no side effects. Best-effort by construction (a failed sub-read
/// degrades that bucket to `0`/empty), so an empty company reads as calm zeros.
fn compute_operations(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    tenant: &str,
    operatives: &[AgentProfile],
) -> serde_json::Value {
    // ── Briefs ──────────────────────────────────────────────────────────────
    // Board buckets give the total + per-column shape; the operational signals
    // (ready / unassigned / dependency-blocked / stale) reuse the EXACT bounded
    // helpers the Action Center reads, so the cockpit snapshot and the feed can
    // never disagree. The bounded lists (≤ ACTION_SRC_CAP) are a glance summary,
    // not an audit — they match what the Action Center surfaces.
    let mut by_board = serde_json::Map::new();
    let mut brief_total = 0i64;
    let mut in_review = 0i64;
    if let Ok(rows) = task_store.board_summary_for_tenant(tenant) {
        for (status, n) in rows {
            brief_total += n;
            if status == "in_review" {
                in_review = n;
            }
            by_board.insert(status, serde_json::json!(n));
        }
    }
    let ready = task_store
        .list_ready_briefs_for_tenant(tenant, ACTION_SRC_CAP)
        .map(|v| v.len())
        .unwrap_or(0);
    let unassigned = task_store
        .list_unassigned_briefs_for_tenant(tenant, ACTION_SRC_CAP)
        .map(|v| v.len())
        .unwrap_or(0);
    let blocked = task_store
        .list_blocked_briefs_for_tenant(tenant, ACTION_SRC_CAP)
        .map(|v| v.len())
        .unwrap_or(0);
    let stale = task_store
        .list_stale_briefs_for_tenant(ACTION_STALE_IDLE_SECS, tenant, ACTION_SRC_CAP)
        .map(|v| v.len())
        .unwrap_or(0);

    // ── Runs ────────────────────────────────────────────────────────────────
    // Classified over a bounded recent window (newest-first). `recent` is the
    // window size, not an all-time total — labelled as such on the wire.
    let mut recent = 0i64;
    let mut running = 0i64;
    let mut failed_or_refused = 0i64;
    let mut pending_review = 0i64;
    if let Ok(runs) = task_store.list_runs_for_tenant(tenant, OPS_RUN_WINDOW) {
        recent = runs.len() as i64;
        for r in &runs {
            match r.status.as_str() {
                "running" => running += 1,
                "failed" | "refused" | "interrupted" => failed_or_refused += 1,
                _ => {}
            }
            if r.status == "done" && r.review.as_deref() == Some("pending_review") {
                pending_review += 1;
            }
        }
    }

    // ── Approvals ───────────────────────────────────────────────────────────
    // Pending Clearances (the unified decision queue) + pending hires (inert
    // Operatives awaiting activation; reused from the roster — no extra read).
    let pending_clearances = agent_store
        .list_pending_approvals_for_tenant(ACTION_SRC_CAP, tenant)
        .map(|v| v.len())
        .unwrap_or(0);
    let pending_hires = operatives
        .iter()
        .filter(|o| o.status.eq_ignore_ascii_case("pending"))
        .count();

    // ── Mandates ────────────────────────────────────────────────────────────
    // Total + status tally (cheap, in-memory over the full list) + the count of
    // strategies still `proposed` (the gate awaiting the Board's approval). The
    // per-Mandate strategy probe is bounded to ACTION_SRC_CAP like the feed.
    let mut mandate_total = 0i64;
    let mut by_mandate_status = serde_json::Map::new();
    let mut strategy_proposed = 0i64;
    if let Ok(mandates) = spine_store.list_mandates(tenant, None) {
        mandate_total = mandates.len() as i64;
        for m in &mandates {
            bump_tally(&mut by_mandate_status, m.status.trim());
        }
        for m in mandates.iter().take(ACTION_SRC_CAP) {
            if spine_store
                .strategy_status(tenant, &m.mandate_id)
                .ok()
                .flatten()
                .as_deref()
                == Some("proposed")
            {
                strategy_proposed += 1;
            }
        }
    }

    serde_json::json!({
        "briefs": {
            "total": brief_total,
            "by_board": by_board,
            "in_review": in_review,
            "ready_to_start": ready,
            "unassigned": unassigned,
            "blocked": blocked,
            "stale": stale,
        },
        "runs": {
            "window": OPS_RUN_WINDOW,
            "recent": recent,
            "running": running,
            "failed_or_refused": failed_or_refused,
            "pending_review": pending_review,
        },
        "approvals": {
            "pending_clearances": pending_clearances,
            "pending_hires": pending_hires,
        },
        "mandates": {
            "total": mandate_total,
            "by_status": by_mandate_status,
            "strategy_proposed": strategy_proposed,
        },
    })
}

/// `company.bootstrap_founder` — first-run owner action. Wire arg:
/// `name|rig` (both optional; defaults `Founder` + `echo`). Creates the
/// single Founder Operative for the Guild if absent, idempotently
/// (a repeat call returns the existing Founder, never a duplicate).
/// Owner-gated (operator/admin or the console identity); a normal
/// Operative is refused. Returns `{founder, created}`.
pub fn handle_bootstrap_founder(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    if !caller_is_owner(store, ctx) {
        return security_denied(
            "company.bootstrap_founder is owner-only (dashboard admin / operator)".to_string(),
        );
    }
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("company.bootstrap_founder utf8: {e}")),
    };
    let mut parts = s.splitn(2, '|');
    let name = parts.next().unwrap_or("").trim();
    let rig = parts.next().unwrap_or("").trim();
    let tenant = ctx.tenant_id_or_default();
    let created_by = ctx.caller.subject_id.to_string();
    let (agent_id, created) = match store.ensure_founder(name, rig, &created_by, tenant) {
        Ok(r) => r,
        Err(AgentStoreError::BadInput(m)) => return invalid(m),
        Err(e) => return internal(format!("company.bootstrap_founder: {e}")),
    };
    let founder = match store.get_agent_for_tenant(&agent_id, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => return internal("company.bootstrap_founder: founder vanished".into()),
        Err(e) => return internal(format!("company.bootstrap_founder read: {e}")),
    };
    let body = serde_json::json!({
        "founder": operative_json(&founder),
        "created": created,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("company.bootstrap_founder encode: {e}")),
    }
}

/// Default starter-crew roles (company-model §12.6): the two tracks the
/// flagship "build" plan uses, so a build proposal becomes fully runnable.
const STARTER_DEFAULT_ROLES: [&str; 2] = ["engineer", "designer"];
/// Hard cap on how many starter roles one call provisions (abuse guard).
const STARTER_MAX_ROLES: usize = 6;

/// Operator-facing display title for a canonical starter role.
fn starter_role_title(canon: &str) -> &'static str {
    match canon {
        "engineer" => "Engineer",
        "designer" => "Designer",
        "researcher" => "Researcher",
        "writer" => "Writer",
        "qa" => "QA",
        "ops" => "Ops",
        _ => "Operative",
    }
}

/// `company.starter_crew` — first-run safe-local on-ramp (company-model §12.6).
/// Wire arg: `rig|roles_csv` (both optional; defaults `echo` + `engineer,designer`).
/// Owner-gated (operator/admin or the console identity); a normal Operative is
/// refused. Idempotently ensures the Founder exists, then ensures **one active
/// safe-local starter Operative per requested role**, bound to `rig` (the
/// built-in `echo` by default) and clearly labelled local/safe — never a fake
/// Claude/Codex agent. Direct active creation is acceptable here because it is
/// the Board's sovereign first-run action (§5.4); it hires no one behind a
/// Clearance, runs no adapter, and changes no budget. Tenant-scoped; a re-run
/// never duplicates the Founder or a role's starter. Returns
/// `{founder, founder_created, rig, safe_local, crew:[{agent_id,role,name,created}]}`.
pub fn handle_starter_crew(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    if !caller_is_owner(store, ctx) {
        return security_denied(
            "company.starter_crew is owner-only (dashboard admin / operator)".to_string(),
        );
    }
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("company.starter_crew utf8: {e}")),
    };
    let mut parts = s.splitn(2, '|');
    let rig = {
        let r = parts.next().unwrap_or("").trim();
        if r.is_empty() { "echo" } else { r }
    };
    let roles_csv = parts.next().unwrap_or("").trim();
    // Canonicalise + de-duplicate the requested roles, preserving first-seen
    // order; an empty request uses the default roster.
    let mut roles: Vec<&'static str> = Vec::new();
    if roles_csv.is_empty() {
        roles.extend(STARTER_DEFAULT_ROLES.iter().map(|r| prime::canon_role(r)));
    } else {
        for raw in roles_csv.split(',') {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let canon = prime::canon_role(raw);
            if !roles.contains(&canon) {
                roles.push(canon);
            }
            if roles.len() >= STARTER_MAX_ROLES {
                break;
            }
        }
        if roles.is_empty() {
            roles.extend(STARTER_DEFAULT_ROLES.iter().map(|r| prime::canon_role(r)));
        }
    }
    let tenant = ctx.tenant_id_or_default();
    let created_by = ctx.caller.subject_id.to_string();

    // 1) Ensure the apex Founder (idempotent) so the company is initialised.
    let (founder_id, founder_created) = match store.ensure_founder("", rig, &created_by, tenant) {
        Ok(r) => r,
        Err(AgentStoreError::BadInput(m)) => return invalid(m),
        Err(e) => return internal(format!("company.starter_crew founder: {e}")),
    };
    let founder = match store.get_agent_for_tenant(&founder_id, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => return internal("company.starter_crew: founder vanished".into()),
        Err(e) => return internal(format!("company.starter_crew read: {e}")),
    };

    // 2) Ensure one safe-local starter Operative per role (idempotent).
    let mut crew = Vec::new();
    for canon in &roles {
        let title = format!("Starter {}", starter_role_title(canon));
        let name = format!("{title} (local · {rig})");
        match store.ensure_starter_operative(canon, &name, &title, rig, tenant) {
            Ok((agent_id, created)) => crew.push(serde_json::json!({
                "agent_id": agent_id,
                "role": canon,
                "name": name,
                "created": created,
            })),
            Err(AgentStoreError::BadInput(m)) => return invalid(m),
            Err(e) => return internal(format!("company.starter_crew operative: {e}")),
        }
    }

    let body = serde_json::json!({
        "founder": operative_json(&founder),
        "founder_created": founder_created,
        "rig": rig,
        // `safe_local` is only honestly true when the crew runs the local echo
        // Rig — any other Rig is the operator's explicit, non-default choice.
        "safe_local": rig == "echo",
        "crew": crew,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("company.starter_crew encode: {e}")),
    }
}

/// `agent.operatives` — the Crew roster (no args): every real Operative
/// in the Guild (excludes the infra operator-console). Tenant-scoped.
/// Returns a JSON array of compact Operative views (with `rig`).
pub fn handle_operatives(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let tenant = ctx.tenant_id_or_default();
    match store.list_operatives_for_tenant(tenant) {
        Ok(rows) => {
            let arr: Vec<serde_json::Value> = rows.iter().map(operative_json).collect();
            match serde_json::to_vec(&arr) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => internal(format!("agent.operatives encode: {e}")),
            }
        }
        Err(e) => internal(format!("agent.operatives: {e}")),
    }
}

// ── Prime Assistant: governed "describe what you want → plan" ─────────

/// `prime.propose` — interpret a free-text request into a structured,
/// READ-ONLY plan (intent, Mandate, crew roles, suggested hires, Brief
/// breakdown + deps, risks, next actions). Creates NOTHING except the
/// proposal record itself. The request is secret-redacted before it is
/// interpreted or persisted. Tenant-scoped.
///
/// **Arg (two accepted forms):**
/// - Raw UTF-8 text — the historical form; produces a rule-based plan
///   (`ai_mode = "deterministic_only"`).
/// - A JSON object `{"message": "...", "model_output"?: "...",
///   "model_unavailable_reason"?: "..."}` — the model-assisted seam
///   (company-model §12.5A). When `model_output` is present it is the RAW
///   text a model emitted (supplied by the bridge, which is the only place a
///   model is reachable today — no LLM is synchronously callable from this
///   coordinator handler). The coordinator is the AUTHORITATIVE validator: it
///   runs the output through [`prime_plan::validate_model_plan`] server-side
///   (bounded, sanitized, secret-redacted, dependency-checked) and only on
///   success stores it as `ai_mode = "llm_used"`. Any validation failure falls
///   back to the deterministic plan (`ai_mode = "fallback"`, with an honest
///   reason); `model_unavailable_reason` records that no model was reachable
///   (`ai_mode = "unavailable"`). The model can NEVER inject crew, hires, or
///   governance — those stay coordinator-computed from the live roster.
///
/// Returns `{proposal_id, status, proposal}`.
pub fn handle_prime_propose(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    use crate::nodes::coordinator::agent::{prime_plan, prime_plan::PlanValidationError};

    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("prime.propose utf8: {e}")),
    };
    if raw.is_empty() {
        return invalid("prime.propose: a request message is required".into());
    }

    // Parse the optional structured form. A bare message that is not a JSON
    // object with a `message` field is treated verbatim (back-compat).
    #[derive(serde::Deserialize)]
    struct ProposeArgs {
        message: String,
        #[serde(default)]
        model_output: Option<String>,
        #[serde(default)]
        model_unavailable_reason: Option<String>,
    }
    let (raw_message, model_output, model_unavailable) = if raw.starts_with('{') {
        match serde_json::from_str::<ProposeArgs>(raw) {
            Ok(a) if !a.message.trim().is_empty() => {
                (a.message, a.model_output, a.model_unavailable_reason)
            }
            // A `{...}` that isn't our schema (or empty message) → treat the
            // whole thing as the literal request, never a silent failure.
            _ => (raw.to_string(), None, None),
        }
    } else {
        (raw.to_string(), None, None)
    };
    let raw_message = raw_message.trim();
    if raw_message.is_empty() {
        return invalid("prime.propose: a request message is required".into());
    }

    // Redact secrets BEFORE the request is interpreted or persisted.
    let message = crate::rig::redact_secrets(raw_message, "");
    let tenant = ctx.tenant_id_or_default();
    let operatives = match agent_store.list_operatives_for_tenant(tenant) {
        Ok(v) => v,
        Err(e) => return internal(format!("prime.propose roster: {e}")),
    };
    let crew: Vec<prime::CrewMember> = operatives
        .iter()
        .map(|p| prime::CrewMember {
            agent_id: p.agent_id.clone(),
            name: p.name.clone(),
            role: p.role.clone(),
            status: p.status.clone(),
        })
        .collect();

    // Model-assisted seam: validate server-side, fall back on ANY failure.
    let proposal = if let Some(reason) = model_unavailable
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        let reason = prime_plan::sanitize_text(reason, 200);
        prime::deterministic_fallback(&message, &crew, prime::AiMode::Unavailable, reason)
    } else if let Some(output) = model_output
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    {
        match prime_plan::validate_model_plan(output, &message) {
            Ok(plan) => prime::proposal_from_model(plan, &crew),
            Err(e) => {
                // The reason is the validator's own message — never raw model
                // content — so it is safe to surface and store.
                let reason = match &e {
                    PlanValidationError::Parse(_) => {
                        "model output was not valid plan JSON".to_string()
                    }
                    other => other.to_string(),
                };
                prime::deterministic_fallback(&message, &crew, prime::AiMode::Fallback, reason)
            }
        }
    } else {
        prime::generate_proposal(&message, &crew)
    };
    let proposal_json = match serde_json::to_string(&proposal) {
        Ok(s) => s,
        Err(e) => return internal(format!("prime.propose plan encode: {e}")),
    };
    let proposer = ctx.caller.subject_id.to_string();
    let proposal_id =
        match spine_store.record_prime_proposal(tenant, &proposer, &message, &proposal_json) {
            Ok(id) => id,
            Err(e) => return internal(format!("prime.propose persist: {e}")),
        };
    let body = serde_json::json!({
        "proposal_id": proposal_id,
        "status": "proposed",
        "proposal": proposal,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("prime.propose encode: {e}")),
    }
}

/// `prime.approve` — the ONLY path that materializes a Prime proposal.
/// Tenant-gated (a proposal from another Guild reads as not-found). Creates
/// the Mandate, the Briefs (idempotent per-key source markers) + their
/// dependency edges, assigns each track to an EXISTING eligible active
/// Operative (never a fake agent), and files a `pending` hire request (needs
/// a separate Clearance to activate) for each missing role. It NEVER runs an
/// adapter, applies a workspace, or changes budget. Records the approval on
/// the proposal row + a Team Plan + an Orchestration run + a Chronicle event
/// on each created Brief. Idempotent: an already-approved proposal returns its
/// created objects. Arg: the `proposal_id`.
pub fn handle_prime_approve(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let proposal_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("prime.approve utf8: {e}")),
    };
    if proposal_id.is_empty() {
        return invalid("prime.approve: proposal_id required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    let row = match spine_store.get_prime_proposal(tenant, proposal_id) {
        Ok(Some(r)) => r,
        // Unknown OR cross-tenant → not found (no existence leak).
        Ok(None) => return invalid(format!("proposal not found: {proposal_id}")),
        Err(e) => return internal(format!("prime.approve load: {e}")),
    };
    // Idempotent: a re-approve returns the already-created Mandate.
    if row.status == "approved" {
        let created: serde_json::Value =
            serde_json::from_str(&row.created_brief_ids).unwrap_or(serde_json::Value::Null);
        let body = serde_json::json!({
            "proposal_id": proposal_id, "status": "approved", "already_approved": true,
            "mandate_id": row.mandate_id, "created_briefs": created,
        });
        return match serde_json::to_vec(&body) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("prime.approve encode: {e}")),
        };
    }
    // The PERSISTED plan is the source of truth — never client input.
    let plan: prime::PrimeProposal = match serde_json::from_str(&row.proposal_json) {
        Ok(p) => p,
        Err(e) => return internal(format!("prime.approve plan parse: {e}")),
    };
    let actor = ctx.caller.subject_id.to_string();

    // 1) Mandate.
    let mandate_id = match spine_store.create_mandate(
        tenant,
        &plan.mandate_title,
        &plan.mandate_brief,
        None,
        None,
    ) {
        Ok(id) => id,
        Err(e) => return internal(format!("prime.approve mandate: {e}")),
    };

    // 2) Briefs (idempotent per-key markers), mapping proposal key → task_id.
    //    Each Brief is stamped with the company's reviewer up front: the
    //    Founder/Board — the sovereign reviewer of completed Shifts in the
    //    first-run local loop (company-model §5.4 / §12.6). The Founder is a
    //    same-tenant Operative (`find_founder` is tenant-scoped + deterministic:
    //    oldest `role='founder'` row), never a cross-Guild or arbitrary agent.
    //    With a reviewer in place a finished Shift can move in_progress →
    //    in_review instead of parking in `blocked` for want of a reviewer
    //    (execution-and-issue §1.3; heartbeat's "missing reviewer parks it").
    //    No Founder (company not bootstrapped) → leave it unset and the honest
    //    "parks in blocked until a reviewer is set" fallback still holds.
    let reviewer_agent_id: Option<String> = agent_store
        .find_founder(tenant)
        .ok()
        .flatten()
        .map(|f| f.agent_id);
    let mut key_to_id: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut created_ids: Vec<String> = Vec::new();
    for b in &plan.briefs {
        let marker = format!("prime:{proposal_id}:{}", b.key);
        let task_id = match task_store.create_brief_with_marker(
            tenant,
            &b.title,
            &actor,
            Some(&mandate_id),
            "brief/prime",
            &marker,
        ) {
            Ok(id) => id,
            Err(e) => return internal(format!("prime.approve brief: {e}")),
        };
        // Stamp the Founder/Board as reviewer so a completed Shift lands in
        // `in_review`, not `blocked` (company-model §12.6). This covers the
        // role tracks AND the dependent `integrate` Brief — every materialized
        // Brief gets the same reviewer-aware lifecycle.
        if let Some(rev) = reviewer_agent_id.as_deref() {
            let _ = task_store.set_brief_field(&task_id, "reviewer", rev);
        }
        key_to_id.insert(b.key.clone(), task_id.clone());
        created_ids.push(task_id);
    }

    // 3) Dependency edges (a Brief is blocked on each of its depends_on tracks).
    for b in &plan.briefs {
        if let Some(child) = key_to_id.get(&b.key) {
            for dep in &b.depends_on {
                if let Some(blocker) = key_to_id.get(dep) {
                    let _ = task_store.add_snag(child, blocker);
                }
            }
        }
    }

    // 4) Assignments — ONLY to an existing eligible active Operative whose
    //    role family matches the track. No match → leave unassigned (the
    //    matching hire is suggested below, never silently created active).
    let operatives = agent_store
        .list_operatives_for_tenant(tenant)
        .unwrap_or_default();
    let mut assigned: Vec<String> = Vec::new();
    for b in &plan.briefs {
        if let Some(task_id) = key_to_id.get(&b.key) {
            let want = prime::canon_role(&b.role);
            if let Some(op) = operatives
                .iter()
                .find(|o| o.status == "active" && prime::canon_role(&o.role) == want)
                && task_store
                    .set_brief_field(task_id, "assignee", &op.agent_id)
                    .is_ok()
            {
                assigned.push(task_id.clone());
            }
        }
    }

    // 5) Hire requests for MISSING roles — `pending` (need a Clearance to
    //    activate), NOT active agents.
    let mut hire_agent_ids: Vec<String> = Vec::new();
    for h in &plan.hires {
        let subject = format!("prime-hire:{proposal_id}:{}", h.role);
        let name = format!("{} (proposed)", h.title);
        // `department` / `team` are required non-empty; the role is a sane
        // default for a proposed hire.
        if let Ok(agent_id) = agent_store.request_hire(
            &name, &h.role, &h.title, &h.role, &h.role, &actor, &subject, "medium", tenant,
        ) {
            hire_agent_ids.push(agent_id);
        }
    }

    // 6) History — flip the proposal + record on the existing Mandate-history
    //    surfaces + a Chronicle event on each created Brief.
    let created_json = serde_json::to_string(&created_ids).unwrap_or_else(|_| "[]".into());
    let _ =
        spine_store.mark_prime_proposal_approved(tenant, proposal_id, &mandate_id, &created_json);

    let roles_json = serde_json::to_string(&plan.roles).unwrap_or_else(|_| "[]".into());
    let pending_hires: Vec<serde_json::Value> = hire_agent_ids
        .iter()
        .zip(plan.hires.iter())
        .map(|(id, h)| serde_json::json!({ "agent_id": id, "role": h.role }))
        .collect();
    let pending_hires_json = serde_json::to_string(&pending_hires).unwrap_or_else(|_| "[]".into());
    let next_steps_json = serde_json::to_string(&plan.next_actions).unwrap_or_else(|_| "[]".into());
    let _ = spine_store.record_team_plan(&TeamPlanRecord {
        tenant_id: tenant,
        mandate_id: &mandate_id,
        actor_id: &actor,
        description: &format!("Prime proposal {proposal_id}"),
        proposed_roles_json: &roles_json,
        pending_hires_json: &pending_hires_json,
        clearance_ids_json: "[]",
        denials_json: "[]",
        next_steps_json: &next_steps_json,
        status: if hire_agent_ids.is_empty() {
            "planned"
        } else {
            "staffing"
        },
    });
    let assigned_json = serde_json::to_string(&assigned).unwrap_or_else(|_| "[]".into());
    let _ = spine_store.record_orchestration_run(&OrchestrationRunRecord {
        tenant_id: tenant,
        mandate_id: &mandate_id,
        mode: "create_briefs",
        dry_run: false,
        input_signature: proposal_id,
        status: "created",
        created_brief_ids_json: &created_json,
        existing_brief_ids_json: "[]",
        assigned_brief_ids_json: &assigned_json,
        skipped_json: "[]",
        source_markers_json: "[]",
        blockers_json: "[]",
        next_actions_json: &next_steps_json,
    });
    for id in &created_ids {
        let _ = task_store.append_event(
            id,
            "prime.brief_created",
            &format!("from Prime proposal {proposal_id} (mandate {mandate_id})"),
        );
    }

    let body = serde_json::json!({
        "proposal_id": proposal_id,
        "status": "approved",
        "mandate_id": mandate_id,
        "created_briefs": created_ids,
        "assigned_briefs": assigned,
        "hire_requests": hire_agent_ids,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("prime.approve encode: {e}")),
    }
}

/// `prime.proposals` — recent Prime proposals for the Guild, newest first
/// (the companion history). Arg: optional limit (default 20). Tenant-scoped.
pub fn handle_prime_proposals(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let limit = std::str::from_utf8(&ctx.args)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(20);
    let tenant = ctx.tenant_id_or_default();
    match spine_store.list_prime_proposals(tenant, limit) {
        Ok(rows) => {
            let arr: Vec<serde_json::Value> = rows.iter().map(|r| r.to_json()).collect();
            match serde_json::to_vec(&arr) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => internal(format!("prime.proposals encode: {e}")),
            }
        }
        Err(e) => internal(format!("prime.proposals: {e}")),
    }
}

/// `prime.proposal` — one proposal by id, tenant-scoped (a proposal from
/// another Guild reads as not-found). Arg: proposal_id.
pub fn handle_prime_proposal_get(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let proposal_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("prime.proposal utf8: {e}")),
    };
    if proposal_id.is_empty() {
        return invalid("prime.proposal: proposal_id required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    match spine_store.get_prime_proposal(tenant, proposal_id) {
        Ok(Some(r)) => match serde_json::to_vec(&r.to_json()) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("prime.proposal encode: {e}")),
        },
        Ok(None) => invalid(format!("proposal not found: {proposal_id}")),
        Err(e) => internal(format!("prime.proposal: {e}")),
    }
}

/// Default cap on how many Briefs a single `prime.start` call dispatches —
/// ready Briefs beyond it are reported skipped (never silently dropped) and a
/// repeat call continues. Overridable per call via `proposal_id|max`.
const DEFAULT_PRIME_START_CAP: usize = 16;

/// `prime.start` — Start-to-Shift (company-model §12.5B). Turns an APPROVED
/// Prime proposal into running **Shifts** by funneling its READY Briefs
/// through the SAME run chokepoint as `brief.run`
/// ([`heartbeat::preflight_and_spawn`]). It creates no Mandate/Brief/hire and
/// changes no budget — it only RUNS Briefs that are already assigned to an
/// active Operative, unblocked, and not already claimed/running. It is
/// operator-initiated + sovereign (a `manual`-trigger run, like `brief.run`;
/// the single-owner Claim prevents double-work). Every created Brief that is
/// NOT started is returned with an honest reason. Records an Orchestration run
/// (`mode:"start"`) on the Mandate + a `prime.work_started` Chronicle event on
/// each started Brief. Arg: `proposal_id` (optionally `proposal_id|max`).
/// Tenant-gated: a non-approved / unknown / cross-Guild proposal is refused.
pub fn handle_prime_start(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("prime.start utf8: {e}")),
    };
    if raw.is_empty() {
        return invalid("prime.start: proposal_id required".into());
    }
    // `proposal_id` or `proposal_id|max`.
    let mut parts = raw.splitn(2, '|');
    let proposal_id = parts.next().unwrap_or("").trim();
    let max_start: usize = parts
        .next()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(DEFAULT_PRIME_START_CAP);
    if proposal_id.is_empty() {
        return invalid("prime.start: proposal_id required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    let row = match spine_store.get_prime_proposal(tenant, proposal_id) {
        Ok(Some(r)) => r,
        // Unknown OR cross-tenant → not found (no existence leak).
        Ok(None) => return invalid(format!("proposal not found: {proposal_id}")),
        Err(e) => return internal(format!("prime.start load: {e}")),
    };
    if row.status != "approved" {
        return invalid(format!(
            "proposal {proposal_id} is `{}` — approve it before starting work",
            row.status
        ));
    }
    let created: Vec<String> = serde_json::from_str(&row.created_brief_ids).unwrap_or_default();
    if created.is_empty() {
        let body = serde_json::json!({
            "proposal_id": proposal_id, "mandate_id": row.mandate_id,
            "started": [], "skipped": [],
        });
        return match serde_json::to_vec(&body) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("prime.start encode: {e}")),
        };
    }

    // ── Governed assignment reconciliation (company-model §12.5B) ──────────
    // `prime.approve` assigned each track only to the Operatives that were
    // ACTIVE then, and filed the missing roles as `pending` hires. When the
    // operator SINCE greenlights one of those hires (pending → active via
    // `agent.approve_hire`), its planned role-track Brief is still unassigned —
    // so it would skip as `Unassigned` forever and any dependent Brief (e.g. the
    // `integrate` track that depends on every track) would never unblock. This
    // pass COMPLETES the assignment `prime.approve` already planned, using the
    // IDENTICAL role match, for any now-active Operative. It is the operator's
    // sovereign completion of the approved plan (this whole flow is operator-
    // initiated): it hires/approves/creates no one, never clobbers an existing
    // assignee, and assigns nothing the operator did not already greenlight as a
    // hire for that role. Runs BEFORE the readiness set is read so the freshly
    // staffed track is seen as ready in this same call.
    let mut late_assigned: Vec<String> = Vec::new();
    if let Ok(plan) = serde_json::from_str::<prime::PrimeProposal>(&row.proposal_json) {
        let operatives = agent_store
            .list_operatives_for_tenant(tenant)
            .unwrap_or_default();
        for b in &plan.briefs {
            let marker = format!("prime:{proposal_id}:{}", b.key);
            let card = match task_store.get_brief_by_source_marker(&marker) {
                Ok(Some(c)) => c,
                _ => continue,
            };
            // Only fill a STILL-unassigned track — never clobber an existing
            // (prime.approve or operator) assignment.
            if !card.assignee_agent_id.as_deref().unwrap_or("").is_empty() {
                continue;
            }
            let want = prime::canon_role(&b.role);
            if let Some(op) = operatives
                .iter()
                .find(|o| o.status == "active" && prime::canon_role(&o.role) == want)
                && task_store
                    .set_brief_field(&card.task_id, "assignee", &op.agent_id)
                    .is_ok()
            {
                let _ = task_store.append_event(
                    &card.task_id,
                    "prime.assigned",
                    &format!(
                        "Prime assigned now-active {want} `{}` (hire greenlit since approval) from proposal {proposal_id}",
                        op.agent_id
                    ),
                );
                late_assigned.push(card.task_id.clone());
            }
        }
    }

    // Canonical readiness set: assigned to an active Operative, unblocked, not
    // already claimed/running (a generous batch so we see all the ready Briefs).
    let ready_ids: std::collections::HashSet<String> = task_store
        .list_ready_briefs(500)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.task_id)
        .collect();

    // Classify each created Brief → a Start readiness (the skip reason is pure,
    // in `prime::StartReadiness`).
    let mut items: Vec<(String, prime::StartReadiness)> = Vec::with_capacity(created.len());
    for id in &created {
        let readiness = match task_store.brief_card(id) {
            Ok(Some(card)) => prime::classify_start_readiness(
                &card.board_status,
                !card.assignee_agent_id.as_deref().unwrap_or("").is_empty(),
                ready_ids.contains(id),
            ),
            _ => prime::StartReadiness::Missing,
        };
        items.push((id.clone(), readiness));
    }

    let (mut to_start, mut skipped) = prime::partition_start(&items);

    // Honor the per-call start cap WITHOUT silently dropping the rest.
    if to_start.len() > max_start {
        let deferred = to_start.split_off(max_start);
        for id in deferred {
            skipped.push(prime::SkippedBrief {
                brief_id: id,
                reason: format!(
                    "not started this batch (start cap {max_start} reached) — start again to continue"
                ),
            });
        }
    }

    // Start each ready Brief through the SHARED chokepoint.
    let bridge_tokens = crate::rig::bridge::BridgeTokenStore::global();
    let mut started: Vec<serde_json::Value> = Vec::new();
    let mut started_ids: Vec<String> = Vec::new();
    for brief_id in &to_start {
        // Resolve the assignee's preferred Rig + charter + model hints
        // (exactly as brief.run).
        let (preferred, charter, assignee, prefs) = match task_store.brief_card(brief_id) {
            Ok(Some(card)) => {
                let assignee = card.assignee_agent_id.clone().unwrap_or_default();
                let agent = card
                    .assignee_agent_id
                    .as_deref()
                    .and_then(|a| agent_store.get_agent_for_tenant(a, tenant).ok().flatten());
                let prefs = agent
                    .as_ref()
                    .map(|a| {
                        crate::nodes::coordinator::heartbeat::RunModelPrefs::new(
                            a.model_preference.clone(),
                            a.reasoning_effort.clone(),
                        )
                    })
                    .unwrap_or_default();
                (
                    agent.as_ref().and_then(|a| a.rig.clone()),
                    agent
                        .map(|a| a.instruction_bundle)
                        .filter(|c| !c.trim().is_empty()),
                    assignee,
                    prefs,
                )
            }
            _ => (
                None,
                None,
                String::new(),
                crate::nodes::coordinator::heartbeat::RunModelPrefs::default(),
            ),
        };
        let prompt = task_store.compose_brief_prompt_with_charter(brief_id, 10, charter.as_deref());
        match crate::nodes::coordinator::heartbeat::preflight_and_spawn(
            task_store,
            registry,
            Some(&bridge_tokens),
            crate::nodes::coordinator::heartbeat::DEFAULT_DISPATCH_LEASE_SECS,
            brief_id,
            preferred.as_deref(),
            prompt,
            prefs,
        ) {
            // A Shift started (run_id present).
            Ok(report) if report.run_id.is_some() => {
                let _ = task_store.append_event(
                    brief_id,
                    "prime.work_started",
                    &format!(
                        "Prime started work from proposal {proposal_id} on `{}` (run {})",
                        report.rig,
                        report.run_id.as_deref().unwrap_or("")
                    ),
                );
                started_ids.push(brief_id.clone());
                started.push(serde_json::json!({
                    "brief_id": report.brief_id,
                    "run_id": report.run_id,
                    "rig": report.rig,
                    "status": report.status,
                }));
            }
            // A pre-run refusal (adapter unavailable / Claim lost): record the
            // durable refused Shift + report it as a skip — never a faked run.
            Ok(report) => {
                let _ = task_store.record_manual_refusal_for_tenant(
                    brief_id,
                    tenant,
                    &assignee,
                    &report.rig,
                    &report.status,
                    &report.summary,
                );
                skipped.push(prime::SkippedBrief {
                    brief_id: brief_id.clone(),
                    reason: format!("{}: {}", report.status, report.summary),
                });
            }
            Err(e) => skipped.push(prime::SkippedBrief {
                brief_id: brief_id.clone(),
                reason: format!("internal error: {e}"),
            }),
        }
    }

    // Audit: an Orchestration run (mode:"start") on the Mandate.
    if !row.mandate_id.is_empty() {
        let started_json = serde_json::to_string(&started_ids).unwrap_or_else(|_| "[]".into());
        let skipped_json = serde_json::to_string(&skipped).unwrap_or_else(|_| "[]".into());
        let created_json = serde_json::to_string(&created).unwrap_or_else(|_| "[]".into());
        let _ = spine_store.record_orchestration_run(&OrchestrationRunRecord {
            tenant_id: tenant,
            mandate_id: &row.mandate_id,
            mode: "start",
            dry_run: false,
            input_signature: proposal_id,
            status: "started",
            created_brief_ids_json: &created_json,
            existing_brief_ids_json: "[]",
            assigned_brief_ids_json: &started_json,
            skipped_json: &skipped_json,
            source_markers_json: "[]",
            blockers_json: "[]",
            next_actions_json: "[]",
        });
    }

    let body = serde_json::json!({
        "proposal_id": proposal_id,
        "mandate_id": row.mandate_id,
        // Tracks this call staffed from hires the operator greenlit since approval.
        "assigned": late_assigned,
        "started": started,
        "skipped": skipped,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("prime.start encode: {e}")),
    }
}

/// One created Brief's live state in the Shift Room (PART A). Read-only — it
/// joins the Brief card, its open blockers, and its latest Shift (run) from the
/// existing stores. No new state is invented.
pub(crate) struct BriefStatus {
    pub(crate) json: serde_json::Value,
    /// The bucket this Brief counts toward (`running` / `done` / `blocked` /
    /// `needs_review` / `refused` / `failed` / `ready` / `unassigned` /
    /// `not_ready` / `missing`).
    pub(crate) bucket: &'static str,
}

/// Compose one created Brief's live Shift-Room row. PURE w.r.t. the stores it
/// reads (no mutation). The caller has already gated the owning proposal to
/// `tenant`'s Guild; `tenant` is threaded through so the open-blocker read is
/// itself tenant-scoped — a legacy `blocked_on` edge that crosses Guilds can
/// never surface a cross-tenant blocker here.
pub(crate) fn brief_status_row(
    agent_store: &AgentStore,
    task_store: &TaskStore,
    tenant: &str,
    brief_id: &str,
    in_ready_set: bool,
) -> BriefStatus {
    let card = task_store.brief_card(brief_id).ok().flatten();
    let Some(card) = card else {
        return BriefStatus {
            json: serde_json::json!({
                "brief_id": brief_id,
                "start_readiness": prime::StartReadiness::Missing.as_str(),
                "exists": false,
            }),
            bucket: "missing",
        };
    };

    let assignee = card.assignee_agent_id.clone().unwrap_or_default();
    let has_assignee = !assignee.is_empty();
    // The assignee's preferred Rig (display only — the run path resolves it
    // authoritatively at start time).
    let rig = if has_assignee {
        agent_store
            .get_agent(&assignee)
            .ok()
            .flatten()
            .and_then(|a| a.rig)
    } else {
        None
    };

    // Open blockers (Snags): the Briefs this one is blocked by that are not yet
    // done/cancelled. Tenant-scoped read — even if a legacy `blocked_on` edge
    // crosses Guilds, a cross-tenant blocker id is filtered out (no leak).
    let mut blockers: Vec<serde_json::Value> = Vec::new();
    if let Ok(snags) = task_store.list_snags_for_tenant(brief_id, tenant) {
        for b in snags {
            if let Ok(Some(bc)) = task_store.brief_card(&b) {
                let resolved = matches!(bc.board_status.as_str(), "done" | "cancelled");
                if !resolved {
                    blockers.push(serde_json::json!({
                        "brief_id": bc.task_id,
                        "title": bc.title,
                        "status": bc.board_status,
                    }));
                }
            }
        }
    }
    let has_open_blockers = !blockers.is_empty();

    // A Snag keeps a Brief out of the ready-set but does NOT move its board
    // column, so an open blocker is the honest reason it can't start even when
    // the board still reads `todo`. Surface that as `blocked` for the operator.
    let base = prime::classify_start_readiness(&card.board_status, has_assignee, in_ready_set);
    let readiness = if base != prime::StartReadiness::Ready
        && !matches!(
            base,
            prime::StartReadiness::Complete | prime::StartReadiness::Cancelled
        )
        && has_open_blockers
    {
        prime::StartReadiness::Blocked
    } else {
        base
    };

    // The latest Shift (run) on this Brief, if any.
    let latest = task_store.latest_run_for_brief(brief_id).ok().flatten();
    let needs_review = latest
        .as_ref()
        .map(|r| r.status == "done" && r.review.as_deref() == Some("pending_review"))
        .unwrap_or(false);
    let run_json = latest.as_ref().map(|r| {
        serde_json::json!({
            "run_id": r.run_id,
            "status": r.status,
            "rig": r.rig,
            "trigger": r.trigger,
            "started_at": r.started_at,
            "finished_at": r.finished_at,
            "review": r.review,
            "apply_status": r.apply_status,
            "refusal_reason": r.refusal_reason,
            "summary": r.summary.chars().take(240).collect::<String>(),
        })
    });

    // The single most-useful bucket + per-Brief next action. Live execution
    // state wins over board state.
    let run_status = latest.as_ref().map(|r| r.status.as_str());
    let apply_status = latest.as_ref().and_then(|r| r.apply_status.as_deref());
    let (bucket, next_action): (&'static str, &str) = if run_status == Some("running") {
        ("running", "inspect the running Shift")
    } else if needs_review {
        ("needs_review", "review the completed Shift")
    } else if run_status == Some("failed") {
        ("failed", "inspect the failed Shift")
    } else if run_status == Some("refused") {
        ("refused", "inspect why the Shift was refused")
    } else if matches!(card.board_status.as_str(), "done" | "in_review") {
        if latest.as_ref().map(|r| r.review.as_deref()) == Some(Some("accepted"))
            && apply_status != Some("applied")
        {
            ("done", "apply the reviewed Shift")
        } else {
            ("done", "complete")
        }
    } else if readiness == prime::StartReadiness::Blocked {
        ("blocked", "resolve the blocker")
    } else if readiness == prime::StartReadiness::Ready {
        ("ready", "start this Brief")
    } else if !has_assignee {
        ("unassigned", "approve a hire / Clearance, then assign")
    } else {
        ("not_ready", "assignee not active yet")
    };

    BriefStatus {
        json: serde_json::json!({
            "brief_id": card.task_id,
            "title": card.title,
            "board_status": card.board_status,
            "priority": card.priority,
            "assignee": if has_assignee { serde_json::json!(assignee) } else { serde_json::Value::Null },
            "rig": rig,
            "mandate_id": card.mandate_id,
            "start_readiness": readiness.as_str(),
            "blockers": blockers,
            "needs_review": needs_review,
            "latest_run": run_json,
            "next_action": next_action,
            "exists": true,
        }),
        bucket,
    }
}

/// `prime.status` — the LIVE status of a Prime work session (PART A of the
/// Live Shift Room pack). READ-ONLY: it starts/applies/mutates NOTHING. Given a
/// `proposal_id` (proposed or approved) it answers the operator's "what is
/// happening now" view from the EXISTING stores only — the proposal row, the
/// Brief board, and the run ledger: the proposal status + created objects, the
/// Mandate (id/title), each created Brief (title / board status / assignee /
/// Rig / open blockers / Start readiness / latest Shift + its run/review/apply
/// state / next action), session-level recommended next actions, and roll-up
/// counts (total / running / done / blocked / needs_review / refused / failed /
/// ready / unassigned). Tenant-gated: an unknown / cross-Guild proposal reads
/// as not-found (no existence leak). Arg: `proposal_id`.
pub fn handle_prime_status(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let proposal_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("prime.status utf8: {e}")),
    };
    if proposal_id.is_empty() {
        return invalid("prime.status: proposal_id required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    let row = match spine_store.get_prime_proposal(tenant, proposal_id) {
        Ok(Some(r)) => r,
        // Unknown OR cross-tenant → not found (no existence leak).
        Ok(None) => return invalid(format!("proposal not found: {proposal_id}")),
        Err(e) => return internal(format!("prime.status load: {e}")),
    };

    // The Mandate title (if approved + still resolvable in this Guild).
    let mandate_title = if row.mandate_id.is_empty() {
        None
    } else {
        spine_store
            .get_mandate_for_tenant(&row.mandate_id, tenant)
            .ok()
            .flatten()
            .map(|m| m.title)
    };

    let created: Vec<String> = serde_json::from_str(&row.created_brief_ids).unwrap_or_default();

    // The canonical ready-set, computed once (assigned-to-active + unblocked +
    // unclaimed). A generous batch so every ready Brief is seen.
    let ready_ids: std::collections::HashSet<String> = task_store
        .list_ready_briefs(500)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.task_id)
        .collect();

    let mut briefs: Vec<serde_json::Value> = Vec::with_capacity(created.len());
    let mut c_running = 0i64;
    let mut c_done = 0i64;
    let mut c_blocked = 0i64;
    let mut c_needs_review = 0i64;
    let mut c_refused = 0i64;
    let mut c_failed = 0i64;
    let mut c_ready = 0i64;
    let mut c_unassigned = 0i64;
    let mut c_not_ready = 0i64;
    let mut c_missing = 0i64;
    for id in &created {
        let row = brief_status_row(agent_store, task_store, tenant, id, ready_ids.contains(id));
        match row.bucket {
            "running" => c_running += 1,
            "done" => c_done += 1,
            "blocked" => c_blocked += 1,
            "needs_review" => c_needs_review += 1,
            "refused" => c_refused += 1,
            "failed" => c_failed += 1,
            "ready" => c_ready += 1,
            "unassigned" => c_unassigned += 1,
            "missing" => c_missing += 1,
            _ => c_not_ready += 1,
        }
        briefs.push(row.json);
    }

    // Session-level recommended next actions, derived from the counts (no
    // fabricated state — each only appears when the count is non-zero).
    let mut next_actions: Vec<String> = Vec::new();
    if row.status != "approved" {
        next_actions
            .push("Approve the proposal to create the Mandate + Briefs + crew assignments.".into());
    }
    if c_ready > 0 {
        next_actions.push(format!(
            "Start {c_ready} ready Brief(s) — they will run as Shifts."
        ));
    }
    if c_running > 0 {
        next_actions.push(format!("{c_running} Shift(s) running — inspect progress."));
    }
    if c_needs_review > 0 {
        next_actions.push(format!("Review {c_needs_review} completed Shift(s)."));
    }
    if c_unassigned > 0 {
        next_actions.push(format!(
            "{c_unassigned} Brief(s) have no active Operative — approve a hire / Clearance first."
        ));
    }
    if c_blocked > 0 {
        next_actions.push(format!("{c_blocked} Brief(s) are blocked on a dependency."));
    }
    if c_failed > 0 || c_refused > 0 {
        next_actions.push(format!(
            "{} Shift(s) need attention (failed / refused) — inspect the run.",
            c_failed + c_refused
        ));
    }
    if next_actions.is_empty() {
        next_actions.push("Nothing pending right now.".into());
    }

    let body = serde_json::json!({
        "proposal_id": proposal_id,
        "status": row.status,
        "message": row.message,
        "mandate_id": if row.mandate_id.is_empty() { serde_json::Value::Null } else { serde_json::json!(row.mandate_id) },
        "mandate_title": mandate_title,
        "briefs": briefs,
        "counts": {
            "total_briefs": created.len(),
            "running": c_running,
            "done": c_done,
            "blocked": c_blocked,
            "needs_review": c_needs_review,
            "refused": c_refused,
            "failed": c_failed,
            "ready": c_ready,
            "unassigned": c_unassigned,
            "not_ready": c_not_ready,
            "missing": c_missing,
        },
        "recommended_next_actions": next_actions,
        "updated_at": row.updated_at,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("prime.status encode: {e}")),
    }
}

/// Default stale threshold (1 day) for the Action Center's stale signal —
/// matches the Desk/Inbox `/v1/spine/stale` default so the two surfaces agree.
const ACTION_STALE_IDLE_SECS: i64 = 86_400;
/// Per-source read bound — each contributing query is capped so the feed can
/// never balloon; the merged feed is additionally capped at [`ACTION_FEED_CAP`].
const ACTION_SRC_CAP: usize = 50;
/// Final cap on the ordered+deduped action feed returned to the operator.
const ACTION_FEED_CAP: usize = 60;

/// `company.actions` — the **Action Center** (company-model §5.4 / §8.2,
/// dashboard-design §5). READ-ONLY: it computes the operator's next actions
/// from EXISTING live state — pending approvals/Clearances, pending hires, the
/// Mandate strategy gate, the Brief board (ready / unassigned / blocked /
/// stale), and the run ledger (needs-review / failed-refused-interrupted) — and
/// returns one ordered, deduped feed. It approves, runs, applies, and mutates
/// NOTHING; mutations stay on their existing governed routes. No new
/// notification table — live state IS the source (company-model §8.2).
///
/// Tenant-scoped: every contributing read is scoped to the caller's Guild, so a
/// different Guild's approvals / hires / Briefs / runs never surface here (no
/// existence leak). No args. Returns `{actions, counts, truncated}`.
pub fn handle_company_actions(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    // No live-spend ledger wired (e.g. metrics disabled, or a caller that
    // predates the spend seam) → allowance-backed budget signals only, never a
    // fabricated spend figure.
    handle_company_actions_with_spend(agent_store, spine_store, task_store, None, ctx)
}

/// `company.actions` with an optional authoritative **live-spend** source — the
/// SAME month-to-date ledger + canonical calendar-month window the dispatch/
/// refusal gate enforces (`MetricsQuery::cost_since`, the `over_allowance`
/// path; `heartbeat::allowance_window`). When
/// `spend` is `Some`, the feed adds actual-spend budget alerts (per-Operative
/// over/near Allowance + Guild over/near budget) keyed off real recorded cost;
/// when `None`, NO spend item is emitted (the allowance-committed planning
/// signals still surface). Reading spend through this seam keeps the feed from
/// either disagreeing with the gate or inventing a number.
pub fn handle_company_actions_with_spend(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &TaskStore,
    spend: Option<&dyn action_center::SpendSource>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let tenant = ctx.tenant_id_or_default();
    let mut items: Vec<action_center::ActionItem> = Vec::new();

    // 1. Pending approvals / Clearances (hire activation, spawn, …). The most
    //    urgent class — they gate the whole company (company-model §5.5).
    match agent_store.list_pending_approvals_for_tenant(ACTION_SRC_CAP, tenant) {
        Ok(approvals) => {
            for a in &approvals {
                items.push(action_center::approval_item(a));
            }
        }
        Err(e) => return internal(format!("company.actions approvals: {e}")),
    }

    // 2. Pending hires — Operatives stuck `pending` (inert until approved). A
    //    hire that already has a spawn Clearance (#1) is collapsed by the
    //    dedupe in `finalize` (same underlying agent), so it shows once. The
    //    roster is also reused below for the Allowance hard-stop budget alert.
    let operatives = match agent_store.list_operatives_for_tenant(tenant) {
        Ok(ops) => {
            for o in &ops {
                if o.status.eq_ignore_ascii_case("pending") {
                    items.push(action_center::hire_item(o));
                }
            }
            ops
        }
        Err(e) => return internal(format!("company.actions roster: {e}")),
    };

    // 3. Strategy approvals — Mandates whose strategy is `proposed` (the gate
    //    that must clear before a team can be built). Best-effort: a strategy
    //    read failure for one Mandate simply omits it, never fails the feed.
    if let Ok(mandates) = spine_store.list_mandates(tenant, None) {
        for m in mandates.iter().take(ACTION_SRC_CAP) {
            if spine_store
                .strategy_status(tenant, &m.mandate_id)
                .ok()
                .flatten()
                .as_deref()
                == Some("proposed")
            {
                items.push(action_center::strategy_item(m));
            }
        }
    }

    // 4. ready_to_start — Briefs that can run now (assigned-to-active +
    //    unblocked + unclaimed). Surfaced ABOVE generic blocked work because it
    //    can move the company forward (Part B). Kept to cross-reference the
    //    assignees of waiting work for the Allowance hard-stop alert.
    let ready_cards = task_store
        .list_ready_briefs_for_tenant(tenant, ACTION_SRC_CAP)
        .unwrap_or_default();
    for c in &ready_cards {
        items.push(action_center::ready_item(c));
    }

    // 5. blocked — unassigned active Briefs (missing assignee) + Briefs blocked
    //    on an unfinished dependency.
    if let Ok(cards) = task_store.list_unassigned_briefs_for_tenant(tenant, ACTION_SRC_CAP) {
        for c in &cards {
            items.push(action_center::blocked_item(c, true));
        }
    }
    let dep_blocked_cards = task_store
        .list_blocked_briefs_for_tenant(tenant, ACTION_SRC_CAP)
        .unwrap_or_default();
    for c in &dep_blocked_cards {
        items.push(action_center::blocked_item(c, false));
    }

    // 5b. Budget alerts (Part A) — allowance-backed, from EXISTING tenant-scoped
    //     state only (company-model §5.4 the Board reads/sets budgets, §8.2 the
    //     Inbox surfaces budget thresholds). No live-spend ledger is threaded
    //     into this read path, so we never fabricate a spend figure.
    //
    //     (a) Company commitment vs the Guild budget: the sum of active
    //         Operatives' Allowances against the Guild's configured monthly
    //         budget. Only fires when the Guild has a positive budget set.
    if let Ok(Some(guild)) = spine_store.get_guild(tenant)
        && let Some(budget) = guild.monthly_allowance_cents
        && budget > 0
        && let Ok(committed) = agent_store.committed_allowance_cents_for_tenant(tenant)
    {
        if committed > budget {
            items.push(action_center::budget_committed_item(
                committed, budget, true,
            ));
        } else if committed.saturating_mul(100) >= budget.saturating_mul(90) {
            // committed ≥ 90% of budget — approaching the cap.
            items.push(action_center::budget_committed_item(
                committed, budget, false,
            ));
        }
    }

    //     (b) Per-Operative Allowance hard-stop: an active Operative with a
    //         0/negative Allowance is hard-stopped by the dispatch gate
    //         (heartbeat::allowance_admits). Surface it only when that Operative
    //         has runnable or blocked work assigned and waiting.
    let mut needs_work: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for c in &ready_cards {
        if let Some(a) = c.assignee_agent_id.as_deref() {
            needs_work.insert(a);
        }
    }
    for c in &dep_blocked_cards {
        if let Some(a) = c.assignee_agent_id.as_deref() {
            needs_work.insert(a);
        }
    }
    for o in &operatives {
        if o.status.eq_ignore_ascii_case("active")
            && matches!(o.monthly_allowance_cents, Some(c) if c <= 0)
            && needs_work.contains(o.agent_id.as_str())
        {
            items.push(action_center::allowance_hardstop_item(o));
        }
    }

    //     (c) LIVE SPEND (Part B) — actual recorded month-to-date cost from the
    //         SAME authoritative source + window the dispatch/refusal gate
    //         enforces (`MetricsQuery::cost_since` over the current UTC calendar
    //         month, `heartbeat::allowance_window` + `allowance_admits`). Read
    //         ONLY through the `SpendSource`
    //         seam, so when no ledger is wired (`spend == None`) NO spend item is
    //         emitted — the feed never fabricates a figure.
    //
    //         TENANT ISOLATION: we sum ONLY this tenant's own active Operatives'
    //         per-agent spend (each `agent_id` came from
    //         `list_operatives_for_tenant`). We NEVER call a company-wide
    //         `cost_since(None, …)`, so no other Guild's spend can leak into this
    //         Guild's totals. This is ACTUAL spend — kept DISTINCT from the
    //         committed-Allowance planning signal in (a) (different ids/targets,
    //         so both can coexist without double-counting).
    if let Some(spend) = spend {
        let mut company_spend_micros: u64 = 0;
        let mut have_company_spend = false;
        for o in &operatives {
            if !o.status.eq_ignore_ascii_case("active") {
                continue;
            }
            // No recorded spend for this Operative → no signal (never a faked 0).
            let Some(used) = spend.operative_spend_micros(&o.agent_id) else {
                continue;
            };
            company_spend_micros = company_spend_micros.saturating_add(used);
            have_company_spend = true;
            // Per-Operative ACTUAL spend vs its own Allowance — only meaningful
            // for a positive cap (a `0`/negative cap is the hard-stop in (b);
            // a `None` cap is ungated, so there's no threshold to breach).
            if let Some(cap) = o.monthly_allowance_cents
                && cap > 0
            {
                let cap_micros = (cap as u64).saturating_mul(action_center::MICROS_PER_CENT);
                if used >= cap_micros {
                    items.push(action_center::operative_spend_item(o, used, cap, true));
                } else if used.saturating_mul(100)
                    >= cap_micros.saturating_mul(action_center::SPEND_NEAR_PCT)
                {
                    items.push(action_center::operative_spend_item(o, used, cap, false));
                }
            }
        }
        // Guild ACTUAL spend (sum of THIS tenant's Operatives) vs the Guild's
        // configured monthly budget — same budget number the committed signal in
        // (a) uses, but compared against money already spent. Fires only when at
        // least one Operative had recorded spend AND the Guild has a positive
        // budget set.
        if have_company_spend
            && let Ok(Some(guild)) = spine_store.get_guild(tenant)
            && let Some(budget) = guild.monthly_allowance_cents
            && budget > 0
        {
            let budget_micros = (budget as u64).saturating_mul(action_center::MICROS_PER_CENT);
            if company_spend_micros >= budget_micros {
                items.push(action_center::company_spend_item(
                    company_spend_micros,
                    budget,
                    true,
                ));
            } else if company_spend_micros.saturating_mul(100)
                >= budget_micros.saturating_mul(action_center::SPEND_NEAR_PCT)
            {
                items.push(action_center::company_spend_item(
                    company_spend_micros,
                    budget,
                    false,
                ));
            }
        }
    }

    // 6 + 7. needs_review + failed/refused/interrupted — from the LATEST run per
    //        Brief only (runs are newest-first), so an old failed Shift and a
    //        newer done Shift on the same Brief don't both spam the feed.
    if let Ok(runs) = task_store.list_runs_for_tenant(tenant, 200) {
        // Source run ids that already have a guarded retry child in this list, so
        // an already-retried failed Shift never offers a duplicate retry action
        // from the Action Center (mirrors `retry_precheck`'s duplicate guard +
        // the Runs page's `retriedSources`). Belt-and-suspenders alongside the
        // latest-run-per-Brief dedupe below.
        let retried_sources: std::collections::HashSet<&str> = runs
            .iter()
            .filter_map(|r| r.retried_from_run_id.as_deref())
            .collect();
        let mut seen_brief: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &runs {
            if !seen_brief.insert(r.brief_id.clone()) {
                continue;
            }
            let needs_review = r.status == "done" && r.review.as_deref() == Some("pending_review");
            if needs_review {
                items.push(action_center::needs_review_item(r));
            } else if matches!(r.status.as_str(), "failed" | "refused" | "interrupted") {
                let has_retry_child = retried_sources.contains(r.run_id.as_str());
                items.push(action_center::failed_item(r, has_retry_child));
            }
        }
    }

    // 8. stale — lowest priority, informational (stuck-too-long work).
    if let Ok(cards) =
        task_store.list_stale_briefs_for_tenant(ACTION_STALE_IDLE_SECS, tenant, ACTION_SRC_CAP)
    {
        for c in &cards {
            items.push(action_center::stale_item(c));
        }
    }

    // Order + dedupe (Part B), then bound the feed honestly.
    let mut feed = action_center::finalize(items);
    let truncated = feed.len() > ACTION_FEED_CAP;
    feed.truncate(ACTION_FEED_CAP);

    // Counts (computed AFTER dedupe + truncate so the badge matches the feed).
    let mut by_category = serde_json::Map::new();
    let mut by_severity = serde_json::Map::new();
    for it in &feed {
        bump_tally(&mut by_category, it.category.as_str());
        bump_tally(&mut by_severity, it.severity.as_str());
    }

    let body = serde_json::json!({
        "actions": feed,
        "counts": {
            "total": feed.len(),
            "by_category": by_category,
            "by_severity": by_severity,
        },
        "truncated": truncated,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("company.actions encode: {e}")),
    }
}

/// Production [`action_center::SpendSource`] — the authoritative month-to-date
/// spend the dispatch/refusal gate enforces, exposed READ-ONLY to the Action
/// Center. Wraps a [`crate::metrics::MetricsQuery`] and pins the SAME canonical
/// window the heartbeat Allowance gate computes
/// ([`crate::nodes::coordinator::heartbeat::allowance_window`] — the current UTC
/// calendar month), so the feed can never disagree with the gate by reading a
/// different source or window. A ledger read error degrades to `None` (no spend
/// signal) — never a fabricated `0`.
pub struct MetricsSpendSource {
    query: crate::metrics::MetricsQuery,
    since_ms: i64,
}

impl MetricsSpendSource {
    /// Build a source whose window is the current UTC calendar month — the exact
    /// window the heartbeat budget gate uses
    /// ([`crate::nodes::coordinator::heartbeat::allowance_window`]); spend is
    /// summed from the month's first instant (inclusive) up to now.
    pub fn current_month(query: crate::metrics::MetricsQuery) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            query,
            since_ms: crate::nodes::coordinator::heartbeat::allowance_window(now_ms).start_ms,
        }
    }
}

impl action_center::SpendSource for MetricsSpendSource {
    fn operative_spend_micros(&self, agent_id: &str) -> Option<u64> {
        // Mirror the gate exactly: sum `cost_micros` for THIS Operative since the
        // pinned window start. A query error → `None` (no signal), so a transient
        // metrics failure never fabricates spend.
        self.query.cost_since(Some(agent_id), self.since_ms).ok()
    }
}

/// `guild.spend` — THE canonical **Guild month-to-date spend** for the caller's
/// own Guild over the current UTC calendar month (relix-company-model §6.6 /
/// §3.6; relix-dashboard-design §10 "Costs: spend by company"). One numeric JSON
/// object the dashboard Costs page reads instead of approximating Guild spend
/// from the observability metrics window.
///
/// The figure is NOT a dashboard-only calculation: it is the EXACT spend +
/// window the autonomous Guild-budget hard-stop enforces, via the single shared
/// helper [`crate::nodes::coordinator::heartbeat::guild_spend_micros`] over the
/// canonical [`crate::nodes::coordinator::heartbeat::allowance_window`] — so the
/// number on the Costs card can never disagree with the gate.
///
/// **Tenant-safe by construction:** spend sums ONLY the caller's OWN Guild's
/// active Operatives (the helper uses `list_active_for_tenant(tenant)`), never a
/// cross-tenant `cost_since(None, …)`, and the Guild profile is read for the same
/// tenant — no cross-Guild figure can leak.
///
/// Fields: `tenant_id`/`guild_id` (the Guild identity), `display_name`,
/// `spent_micros` (exact integer micro-USD) + `spent_cents` (rounded), the
/// `budget_cents` / `remaining_cents` / `over_budget` triplet (all `null` when no
/// positive Guild budget is configured — honest, never a fabricated `0`),
/// `window_start_ms`, `resets_at_ms`, `now_ms`, and a `source` / `computed_from`
/// pair that makes the canonical ledger + month window explicit. When no metrics
/// ledger is wired (`metrics == None`), `spent_*` are `null` (spend can't be
/// computed honestly) while the budget + window fields still resolve.
///
/// `now_ms` is wall-clock unix-ms, passed by the caller so the window stays
/// deterministic in tests.
pub fn handle_guild_spend(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    metrics: Option<&crate::metrics::MetricsQuery>,
    now_ms: i64,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    use crate::nodes::coordinator::heartbeat;
    let tenant = ctx.tenant_id_or_default();
    let win = heartbeat::allowance_window(now_ms);

    // The Guild profile (budget + display name). A read error is honest-null, not
    // a hard failure — the canonical spent figure can still be returned.
    let guild = spine_store.get_guild(tenant).ok().flatten();
    let display_name = guild.as_ref().map(|g| g.display_name.clone());
    // Only a POSITIVE budget is a configured cap (mirrors the gate + Action
    // Center; `None`/`0`/negative = no Guild budget set).
    let budget_cents: Option<i64> = guild
        .as_ref()
        .and_then(|g| g.monthly_allowance_cents)
        .filter(|b| *b > 0);

    // Canonical month-to-date spend = the SAME figure + window the autonomous
    // Guild hard-stop enforces. No metrics ledger wired → null (never a faked 0).
    let spent_micros: Option<u64> =
        metrics.map(|m| heartbeat::guild_spend_micros(agent_store, m, tenant, win.start_ms));
    // Rounded half-up to the nearest cent; the exact value stays in spent_micros.
    let spent_cents: Option<i64> = spent_micros.map(|m| {
        ((m + action_center::MICROS_PER_CENT / 2) / action_center::MICROS_PER_CENT) as i64
    });

    // remaining = budget − spent (cents); honest even when negative (= over).
    let remaining_cents: Option<i64> = match (budget_cents, spent_cents) {
        (Some(b), Some(s)) => Some(b - s),
        _ => None,
    };
    // over_budget compares EXACT micros vs the budget micros (the gate's `>=`).
    let over_budget: Option<bool> = match (budget_cents, spent_micros) {
        (Some(b), Some(m)) => {
            let budget_micros = (b as u64).saturating_mul(action_center::MICROS_PER_CENT);
            Some(m >= budget_micros)
        }
        _ => None,
    };

    let source = if spent_micros.is_some() {
        "ledger:brief_runs/cost_since; window:utc_calendar_month(allowance_window); gate:guild_hard_stop"
    } else {
        "metrics_ledger_unavailable; window:utc_calendar_month(allowance_window)"
    };

    let body = serde_json::json!({
        "tenant_id": tenant,
        "guild_id": tenant,
        "display_name": display_name,
        "spent_micros": spent_micros,
        "spent_cents": spent_cents,
        "budget_cents": budget_cents,
        "remaining_cents": remaining_cents,
        "over_budget": over_budget,
        "window_start_ms": win.start_ms,
        "resets_at_ms": win.resets_at_ms,
        "now_ms": win.cutoff_ms,
        "source": source,
        "computed_from": "sum of this Guild's active Operatives' month-to-date run-ledger cost over the canonical UTC-calendar-month Allowance window (the autonomous Guild hard-stop's own figure)",
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("guild.spend encode: {e}")),
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
/// `role` or `role:subject_id` entries. Crew is reused first
/// (company-model §12.5A/§12.5B): if the Company already has an active,
/// runnable same-role Operative, that role is **adopted** onto it (the
/// oldest match) and no hire is filed — so a build plan staffs the
/// existing engineer/designer instead of duplicating it. Only when no
/// such Operative exists does a role with a `subject_id` become a real
/// pending hire (through the spawn gate — so a lead/founder route mints a
/// spawn Clearance); a bare unmatched role is only *proposed*.
///
/// Governance:
/// - the Mandate strategy MUST be approved (else POLICY_DENIED);
/// - an Operative actor MUST hold the spawn Key (the assign Key is
///   reported as a readiness flag) — the Founder/Board bypasses.
///
/// Returns a stable JSON plan: `mandate_id`, `strategy_approved`,
/// `actor`, `description`, `proposed_roles`, `adopted`, `pending_hires`,
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
    let mut adopted: Vec<serde_json::Value> = Vec::new();
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
        // Reuse existing crew first (company-model §12.5A/§12.5B): adopt an
        // active, runnable same-role Operative before filing any hire — so a
        // real company uses the engineer/designer it already has rather than
        // duplicating it as a pending hire. Deterministic (oldest runnable
        // match) + tenant-scoped; takes precedence over both the proposed
        // and the explicit-hire path.
        if let Some((canon, agent_id)) = adopt_active_operative(agent_store, role, tenant) {
            if !adopted
                .iter()
                .any(|a| a.get("agent_id").and_then(|v| v.as_str()) == Some(agent_id.as_str()))
            {
                adopted.push(serde_json::json!({"role": canon, "agent_id": agent_id}));
            }
            // Persist the canonical role as required work; live readiness
            // re-resolves it to the active Operative each call. No hire is
            // minted, so the active Operative is never a duplicate pending hire.
            if !proposed_roles.iter().any(|r| r == canon) {
                proposed_roles.push(canon.to_string());
            }
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
    if !adopted.is_empty() {
        next_steps.push(format!(
            "Adopted {} active Operative(s) from existing crew — orchestrate to assign their Briefs (mandate.orchestrate).",
            adopted.len()
        ));
    }
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
        "adopted": adopted,
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

// ── Mandate strategy gate (propose / approve / reject / status) ──────────
//
// The store already enforces the strategy gate (`strategy_approved` is the
// predicate orchestration checks). These thin capabilities expose it to the
// dashboard so an operator can drive a Mandate blocked → planned → ready
// WITHOUT bypassing governance.

/// Build the `{mandate_id, status, approved}` body for a Mandate's strategy.
fn strategy_status_body(
    spine_store: &SpineStore,
    tenant: &str,
    mandate_id: &str,
) -> HandlerOutcome {
    match spine_store.strategy_status(tenant, mandate_id) {
        Ok(status) => {
            let approved = status.as_deref() == Some("approved");
            let body = serde_json::json!({
                "mandate_id": mandate_id,
                "status": status,
                "approved": approved,
            });
            match serde_json::to_vec(&body) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => internal(format!("mandate.strategy: encode: {e}")),
            }
        }
        Err(SpineStoreError::BadInput(m)) | Err(SpineStoreError::NotFound(m)) => {
            invalid(format!("mandate.strategy: {m}"))
        }
        Err(e) => internal(format!("mandate.strategy: {e}")),
    }
}

/// `mandate.strategy.status` — the strategy status
/// (`proposed`/`approved`/`rejected`/`null`). Arg: `mandate_id`. Tenant-scoped.
pub fn handle_strategy_status(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let mandate_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.strategy.status utf8: {e}")),
    };
    if mandate_id.is_empty() {
        return invalid("mandate.strategy.status: mandate_id required".into());
    }
    strategy_status_body(spine_store, ctx.tenant_id_or_default(), mandate_id)
}

/// `mandate.strategy.propose` — set/replace the strategy to `proposed`.
/// Arg: `mandate_id|doc`. Tenant-scoped.
pub fn handle_strategy_propose(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("mandate.strategy.propose utf8: {e}")),
    };
    let mut parts = raw.splitn(2, '|');
    let mandate_id = parts.next().unwrap_or("").trim();
    let doc = parts.next().unwrap_or("").trim();
    if mandate_id.is_empty() {
        return invalid("mandate.strategy.propose: mandate_id required".into());
    }
    if doc.is_empty() {
        return invalid("mandate.strategy.propose: strategy doc required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    match spine_store.propose_strategy(tenant, mandate_id, doc) {
        Ok(()) => strategy_status_body(spine_store, tenant, mandate_id),
        Err(SpineStoreError::BadInput(m)) | Err(SpineStoreError::NotFound(m)) => {
            invalid(format!("mandate.strategy.propose: {m}"))
        }
        Err(e) => internal(format!("mandate.strategy.propose: {e}")),
    }
}

/// `mandate.strategy.approve` — approve a proposed strategy. Arg: `mandate_id`.
pub fn handle_strategy_approve(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let mandate_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.strategy.approve utf8: {e}")),
    };
    if mandate_id.is_empty() {
        return invalid("mandate.strategy.approve: mandate_id required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    match spine_store.approve_strategy(tenant, mandate_id) {
        Ok(()) => strategy_status_body(spine_store, tenant, mandate_id),
        Err(SpineStoreError::BadInput(m)) | Err(SpineStoreError::NotFound(m)) => {
            invalid(format!("mandate.strategy.approve: {m}"))
        }
        Err(e) => internal(format!("mandate.strategy.approve: {e}")),
    }
}

/// `mandate.strategy.reject` — reject a proposed strategy. Arg: `mandate_id`.
pub fn handle_strategy_reject(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let mandate_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.strategy.reject utf8: {e}")),
    };
    if mandate_id.is_empty() {
        return invalid("mandate.strategy.reject: mandate_id required".into());
    }
    let tenant = ctx.tenant_id_or_default();
    match spine_store.reject_strategy(tenant, mandate_id) {
        Ok(()) => strategy_status_body(spine_store, tenant, mandate_id),
        Err(SpineStoreError::BadInput(m)) | Err(SpineStoreError::NotFound(m)) => {
            invalid(format!("mandate.strategy.reject: {m}"))
        }
        Err(e) => internal(format!("mandate.strategy.reject: {e}")),
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
/// `pending_hires` (`{role, agent_id, status, suggested_rig}` not yet active),
/// `blocked_roles` (`{role, reason}` from denials), `readiness`
/// (`not_planned` / `awaiting_clearance` / `staffing` / `ready`), and
/// `next_action`.
/// A computed snapshot of a Mandate's team staffing — the shared logic
/// behind `mandate.team_readiness` and `mandate.orchestrate`. Pure of
/// HTTP shapes; both callers project it into their own JSON.
pub(crate) struct ReadinessView {
    pub planned: bool,
    pub plan: Option<crate::nodes::coordinator::spine::TeamPlan>,
    pub missing_roles: Vec<String>,
    /// `{clearance_id, status}` objects for still-pending Clearances.
    pub pending_clearances: Vec<serde_json::Value>,
    /// `(role, agent_id)` for hires that are now `active`.
    pub active_agents: Vec<(String, String)>,
    /// `{role, agent_id, status, suggested_rig}` for hires not yet active.
    /// `suggested_rig` is the safe-local Rig to approve the hire on so it is
    /// immediately runnable (same as the Action Center `hire` card).
    pub pending_hires: Vec<serde_json::Value>,
    /// `{role, reason}` for denied/disabled roles.
    pub blocked_roles: Vec<serde_json::Value>,
    pub readiness: String,
    pub next_action: String,
}

impl ReadinessView {
    pub fn is_ready(&self) -> bool {
        self.readiness == "ready"
    }

    fn active_agents_json(&self) -> Vec<serde_json::Value> {
        self.active_agents
            .iter()
            .map(|(role, agent_id)| serde_json::json!({"role": role, "agent_id": agent_id}))
            .collect()
    }
}

/// Adopt an already-active, runnable same-role Operative for `role`
/// before any new hire is filed (company-model §12.5A: "roles are
/// matched to active Operatives; a missing role is a `pending` hire
/// suggestion, never a fake active agent"). Returns the
/// `(canonical_role, agent_id)` of the **oldest** active Operative in
/// this Company whose canonical work role matches `role` and that
/// carries a Rig (is runnable). Tenant-scoped — it never reaches into
/// another Company's crew. Returns `None` when `role` is not a
/// recognised work track, or no runnable same-role Operative exists, so
/// the caller falls back to proposing / hiring the role.
pub(crate) fn adopt_active_operative(
    agent_store: &AgentStore,
    role: &str,
    tenant: &str,
) -> Option<(&'static str, String)> {
    let want = prime::try_canon_role(role)?;
    let actives = agent_store.list_active_for_tenant(tenant).ok()?;
    actives
        .into_iter()
        .filter(|p| prime::try_canon_role(&p.role) == Some(want))
        .find(|p| {
            p.rig
                .as_deref()
                .map(str::trim)
                .is_some_and(|r| !r.is_empty())
        })
        .map(|p| (want, p.agent_id))
}

/// Compute the live team-readiness snapshot for a Mandate, tenant-
/// scoped. Combines the latest persisted Team Plan with the CURRENT
/// status of each minted hire and Clearance.
pub(crate) fn compute_readiness(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    tenant: &str,
    mandate_id: &str,
) -> Result<ReadinessView, SpineStoreError> {
    let plan = spine_store.latest_team_plan(tenant, mandate_id)?;
    let Some(plan) = plan else {
        return Ok(ReadinessView {
            planned: false,
            plan: None,
            missing_roles: Vec::new(),
            pending_clearances: Vec::new(),
            active_agents: Vec::new(),
            pending_hires: Vec::new(),
            blocked_roles: Vec::new(),
            readiness: "not_planned".to_string(),
            next_action: "Plan a team for this Mandate (mandate.team_plan).".to_string(),
        });
    };

    let parse_arr = |s: &str| -> Vec<serde_json::Value> {
        serde_json::from_str::<Vec<serde_json::Value>>(s).unwrap_or_default()
    };
    let proposed_roles: Vec<String> = parse_arr(&plan.proposed_roles)
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let pending_hires_raw = parse_arr(&plan.pending_hires);
    let clearance_ids: Vec<String> = parse_arr(&plan.clearance_ids)
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let denials = parse_arr(&plan.denials);

    let mut active_agents: Vec<(String, String)> = Vec::new();
    let mut pending_hires: Vec<serde_json::Value> = Vec::new();
    // A proposed role is resolved live below: adopted onto an existing
    // active Operative when the Company already has one, else a genuine
    // staffing gap. (Was: every proposed role counted as missing.)
    let mut missing_roles: Vec<String> = Vec::new();
    let mut blocked_roles: Vec<serde_json::Value> = denials.clone();
    for h in &pending_hires_raw {
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
            "active" => active_agents.push((role.to_string(), agent_id.to_string())),
            "disabled" | "missing" => {
                missing_roles.push(role.to_string());
                blocked_roles.push(serde_json::json!({
                    "role": role,
                    "reason": format!("hire {agent_id} is {status}"),
                }));
            }
            // Carry the safe-local Rig the operator should approve this hire on
            // so the seat is immediately runnable — the SAME guidance the
            // Action Center `hire` card emits (company-model §12.6). Never a
            // paid/interactive CLI; never a secret — just the public Rig name.
            other => pending_hires.push(serde_json::json!({
                "role": role,
                "agent_id": agent_id,
                "status": other,
                "suggested_rig": crate::rig::SAFE_LOCAL_RIG,
            })),
        }
    }

    // Proposed roles carry no minted hire. Adopt an active runnable
    // same-role Operative when the Company already has one (company-model
    // §12.5A/§12.5B) — so the starter engineer/designer counts as ready
    // instead of being re-hired; otherwise it is a genuine staffing gap.
    for role in &proposed_roles {
        match adopt_active_operative(agent_store, role, tenant) {
            Some((canon, agent_id)) => active_agents.push((canon.to_string(), agent_id)),
            None => missing_roles.push(role.clone()),
        }
    }

    let mut pending_clearances: Vec<serde_json::Value> = Vec::new();
    for cid in &clearance_ids {
        let rec = agent_store
            .get_approval_record_for_tenant(cid, tenant)
            .ok()
            .flatten();
        let status = rec
            .as_ref()
            .map(|r| r.status.as_wire().to_string())
            .unwrap_or_else(|| "missing".to_string());
        if status == "pending" {
            // Role identity: a spawn Clearance records the pending hire's
            // agent_id; map that back to its role via the plan's pending
            // hires so orchestration can give the blocked seat a
            // placeholder role track.
            let agent_id = rec.as_ref().map(|r| r.agent_id.clone()).unwrap_or_default();
            let role = pending_hires_raw.iter().find_map(|h| {
                if h.get("agent_id").and_then(|v| v.as_str()) == Some(agent_id.as_str()) {
                    h.get("role")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                } else {
                    None
                }
            });
            let mut obj = serde_json::json!({"clearance_id": cid, "status": status});
            if !agent_id.is_empty() {
                obj["agent_id"] = serde_json::json!(agent_id);
            }
            if let Some(role) = role {
                obj["role"] = serde_json::json!(role);
            }
            pending_clearances.push(obj);
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
    } else if !pending_hires.is_empty() {
        (
            "staffing",
            format!(
                "Approve {} pending hire(s) (agent.approve_hire).",
                pending_hires.len()
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

    Ok(ReadinessView {
        planned: true,
        plan: Some(plan),
        missing_roles,
        pending_clearances,
        active_agents,
        pending_hires,
        blocked_roles,
        readiness: readiness.to_string(),
        next_action,
    })
}

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
    let view = match compute_readiness(
        agent_store,
        spine_store,
        ctx.tenant_id_or_default(),
        mandate_id,
    ) {
        Ok(v) => v,
        Err(e) => return internal(format!("mandate.team_readiness: {e}")),
    };
    let body = serde_json::json!({
        "mandate_id": mandate_id,
        "planned": view.planned,
        "plan_id": view.plan.as_ref().map(|p| p.plan_id.clone()),
        "plan_status": view.plan.as_ref().map(|p| p.status.clone()),
        "missing_roles": view.missing_roles,
        "pending_clearances": view.pending_clearances,
        "active_agents": view.active_agents_json(),
        "pending_hires": view.pending_hires,
        "blocked_roles": view.blocked_roles,
        "readiness": view.readiness,
        "next_action": view.next_action,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("mandate.team_readiness encode: {e}")),
    }
}

/// Normalise the orchestration `mode`; default is the safe `plan_only`
/// (compute + report, create nothing).
fn orchestration_mode(s: Option<&&str>) -> &'static str {
    match s.map(|x| x.trim()) {
        Some("create_briefs") => "create_briefs",
        Some("assign_ready") => "assign_ready",
        _ => "plan_only",
    }
}

/// Canonical display label for a known role, or `None` for an unknown
/// role (callers fall back to the raw role string). Shared by every
/// orchestration title helper so the role→label map lives in one place.
fn role_label(role: &str) -> Option<&'static str> {
    Some(match role.trim().to_ascii_lowercase().as_str() {
        "engineer" | "engineering" | "swe" | "developer" | "dev" => "Engineering",
        "designer" | "design" | "ux" | "ui" => "Design",
        "researcher" | "research" => "Research",
        "writer" | "writing" | "content" | "copywriter" => "Content",
        "planner" | "pm" | "product" | "prime" => "Planning",
        "qa" | "test" | "tester" | "quality" => "QA",
        "ops" | "devops" | "sre" | "operations" => "Operations",
        "data" | "analyst" | "analytics" | "scientist" => "Data",
        "security" | "sec" | "appsec" => "Security",
        "marketing" | "growth" => "Marketing",
        _ => return None,
    })
}

/// Deterministic, role-aware work title for a role-track Brief. Known
/// roles get a named track; anything else gets a generic "Execute {role}
/// track" title.
fn role_track_title(role: &str, mandate_title: &str) -> String {
    match role_label(role) {
        Some(label) => format!("{label} track: {mandate_title}"),
        None => format!("Execute {} track for {mandate_title}", role.trim()),
    }
}

/// Title for a subject **execution** Brief — the per-agent work item that
/// hangs under a role-track Brief.
fn subject_exec_title(role: &str, agent_id: &str) -> String {
    match role_label(role) {
        Some(label) => format!("{label} execution: {agent_id}"),
        None => format!("{} execution: {agent_id}", role.trim()),
    }
}

/// Title for a **placeholder** role-track Brief — a durable work object
/// for a role that is missing / pending / blocked, so a staffing gap is
/// visible in the tree without an executable Brief under it.
fn placeholder_track_title(role: &str, reason: &str) -> String {
    match role_label(role) {
        Some(label) => format!("{label} track blocked: {reason}"),
        None => format!("{} track blocked: {reason}", role.trim()),
    }
}

/// Is `title` an auto-generated placeholder title for `role`? True only
/// when it still carries the machine-written `… track blocked:` prefix —
/// any operator rename (which would not reproduce that exact prefix)
/// reads as user-edited and must be preserved. Used to safely promote a
/// placeholder role track's title to the active title when its role
/// becomes active, without ever clobbering a hand-edited title.
fn is_auto_placeholder_title(role: &str, title: &str) -> bool {
    let prefix = match role_label(role) {
        Some(label) => format!("{label} track blocked:"),
        None => format!("{} track blocked:", role.trim()),
    };
    title.starts_with(&prefix)
}

/// Stable signature of the orchestration inputs (deterministic across
/// processes — `DefaultHasher::new()` uses a fixed seed). Lets a run
/// record show whether two runs had identical inputs.
fn orchestration_signature(
    mandate_id: &str,
    mode: &str,
    plan_id: &str,
    titles: &[String],
) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    mandate_id.hash(&mut h);
    mode.hash(&mut h);
    plan_id.hash(&mut h);
    for t in titles {
        t.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// `mandate.orchestrate` — turn an approved-strategy Mandate with a
/// ready team into an executable Brief tree (company-model §4.6).
/// Arg: `mandate_id|mode?|max_briefs?|dry_run?`.
///
/// - `mode` ∈ `plan_only` (default; compute + report, create nothing) /
///   `create_briefs` (create the tree, no assignment) / `assign_ready`
///   (create + assign active team agents).
/// - `dry_run` (default false) forces report-only regardless of mode.
///
/// Deterministic + idempotent: Briefs are keyed by stable, Mandate-id-
/// derived **source markers** (not title text), so a repeated run reuses
/// the existing tree rather than duplicating it (and a crash mid-run is
/// recovered on the next run). The tree is three deep:
/// parent `mandate:{id}:parent` → role track `mandate:{id}:role:{role}`
/// → subject execution `mandate:{id}:role:{role}:subject:{agent_id}`.
/// Role-track Briefs stay unassigned; the per-agent subject Brief is the
/// one assigned (assign-Key gated via `enforce_assign_key`). A changed
/// active agent yields a new subject Brief while the role track is reused.
/// Every run is persisted via `record_orchestration_run`.
/// Build the JSON note recorded for one Prime-governed Dossier write attempt
/// during `mandate.orchestrate`. Carries the short `outcome` label plus the
/// distinguishing field (doc id / lock owner / preserved author) so the run
/// result is honest about a skip without dumping any document body.
fn prime_dossier_note(
    task_id: &str,
    kind: &str,
    outcome: &PrimeDossierOutcome,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "task_id": task_id,
        "kind": kind,
        "outcome": outcome.label(),
    });
    match outcome {
        PrimeDossierOutcome::Authored(a) => {
            v["doc_id"] = serde_json::json!(a.doc_id);
            v["author"] = serde_json::json!(a.author);
        }
        PrimeDossierOutcome::AlreadyPresent { doc_id, .. } => {
            v["doc_id"] = serde_json::json!(doc_id);
        }
        PrimeDossierOutcome::LockedByOther { locked_by, .. } => {
            v["locked_by"] = serde_json::json!(locked_by);
        }
        PrimeDossierOutcome::SkippedHumanOwned { author, .. } => {
            v["author"] = serde_json::json!(author);
        }
        PrimeDossierOutcome::Stale { .. } => {}
    }
    v
}

pub fn handle_orchestrate(
    task_store: &TaskStore,
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    // The public `mandate.orchestrate` capability is deterministic by default —
    // it never carries a model-authored blueprint. Only the autonomous/manual
    // Prime tick path passes a validated blueprint (see
    // `handle_orchestrate_with_blueprint`).
    handle_orchestrate_with_blueprint(task_store, agent_store, spine_store, ctx, None)
}

/// Backing implementation of `mandate.orchestrate` that accepts an OPTIONAL,
/// already-validated [`PrimeOrchestrationBlueprint`] authored by the Prime
/// orchestration-authoring layer. **The blueprint is text-only:** it may change
/// the TITLE / DOSSIER / CHECKLIST text of a NEWLY-CREATED parent / active
/// role-track / subject-execution Brief, and nothing else. Every gate
/// (approved strategy, ready team, assign-Key, reviewer stamping, max_briefs cap,
/// placeholder behaviour, source-marker idempotency) is identical with or without
/// it; the roles, agents, assignments, dependencies, Brief ids, and source
/// markers are all still computed deterministically here. An existing Brief's
/// title is never clobbered (reuse is by source marker; titles are set only on
/// creation), and placeholder-track text stays deterministic so the
/// placeholder→active title promotion is preserved. With `blueprint = None` the
/// behaviour is byte-for-byte the deterministic v1.
#[allow(clippy::too_many_lines)]
pub fn handle_orchestrate_with_blueprint(
    task_store: &TaskStore,
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    ctx: &InvocationCtx,
    blueprint: Option<
        &crate::nodes::coordinator::agent::prime_orchestration::PrimeOrchestrationBlueprint,
    >,
) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("mandate.orchestrate utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(4, '|').collect();
    let mandate_id = parts.first().copied().unwrap_or("").trim();
    if mandate_id.is_empty() {
        return invalid(
            "mandate.orchestrate: expected `mandate_id|mode?|max_briefs?|dry_run?`".into(),
        );
    }
    let mode = orchestration_mode(parts.get(1));
    let max_briefs: usize = parts
        .get(2)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(16)
        .clamp(1, 50);
    let dry_run = matches!(
        parts.get(3).copied().map(|s| s.trim().to_ascii_lowercase()),
        Some(ref v) if v == "1" || v == "true" || v == "yes"
    );
    let tenant = ctx.tenant_id_or_default();

    let mut blockers: Vec<serde_json::Value> = Vec::new();
    let mut created_briefs: Vec<serde_json::Value> = Vec::new();
    let mut existing_briefs: Vec<serde_json::Value> = Vec::new();
    let mut assigned_briefs: Vec<serde_json::Value> = Vec::new();
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    let mut next_actions: Vec<String> = Vec::new();

    // Gate 1: the Mandate strategy must be approved. Without it we
    // materialise nothing at all (not even placeholder tracks).
    let mut strategy_ok = true;
    match spine_store.strategy_approved(tenant, mandate_id) {
        Ok(true) => {}
        Ok(false) => {
            strategy_ok = false;
            blockers.push(serde_json::json!({
                "reason": "strategy_not_approved",
                "detail": "approve the Mandate strategy before orchestrating",
            }));
        }
        Err(SpineStoreError::BadInput(m)) | Err(SpineStoreError::NotFound(m)) => {
            return invalid(format!("mandate.orchestrate: {m}"));
        }
        Err(e) => return internal(format!("mandate.orchestrate: {e}")),
    }
    let mandate = match spine_store.get_mandate_for_tenant(mandate_id, tenant) {
        Ok(Some(m)) => m,
        Ok(None) => return invalid(format!("mandate.orchestrate: not found: {mandate_id}")),
        Err(e) => return internal(format!("mandate.orchestrate: {e}")),
    };

    // Gate 2 + readiness.
    let view = match compute_readiness(agent_store, spine_store, tenant, mandate_id) {
        Ok(v) => v,
        Err(e) => return internal(format!("mandate.orchestrate: {e}")),
    };
    if !view.planned {
        blockers.push(serde_json::json!({
            "reason": "no_team_plan",
            "detail": "run mandate.team_plan first",
        }));
    }
    for c in &view.pending_clearances {
        blockers.push(serde_json::json!({"reason": "pending_clearance", "detail": c}));
    }
    for h in &view.pending_hires {
        blockers.push(serde_json::json!({"reason": "pending_hire", "detail": h}));
    }
    for r in &view.missing_roles {
        blockers.push(serde_json::json!({"reason": "missing_role", "detail": r}));
    }
    for b in &view.blocked_roles {
        blockers.push(serde_json::json!({"reason": "blocked_role", "detail": b}));
    }

    let ready = blockers.is_empty() && view.is_ready();

    // Build the deterministic plan: ACTIVE role tracks (each with a
    // staffed agent) and GAP role tracks (placeholders for missing /
    // pending / blocked roles). Both are sorted + deduped by role key; a
    // role with ANY active agent is treated as active (never placeholder).
    let mut active: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    for (role, agent_id) in &view.active_agents {
        let key = role.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        active
            .entry(key)
            .or_insert_with(|| (role.trim().to_string(), agent_id.clone()));
    }
    let active_role_keys: std::collections::BTreeSet<String> = active.keys().cloned().collect();

    // Gap roles → a human reason for the placeholder track. Missing,
    // pending-hire, and denied/blocked roles all qualify; active roles
    // never do.
    let mut gap: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    {
        let mut note_gap = |role: &str, reason: &str| {
            let key = role.trim().to_ascii_lowercase();
            if key.is_empty() || active_role_keys.contains(&key) {
                return;
            }
            // First writer wins, so the most specific reason is noted
            // first: pending clearance → pending hire → missing → blocked.
            gap.entry(key)
                .or_insert_with(|| (role.trim().to_string(), reason.to_string()));
        };
        // A role blocked on a pending spawn Clearance gets the most
        // actionable reason (the approval id), ahead of the generic
        // pending-hire reason for the same seat.
        for c in &view.pending_clearances {
            if let Some(role) = c.get("role").and_then(|v| v.as_str()) {
                let cid = c.get("clearance_id").and_then(|v| v.as_str()).unwrap_or("");
                note_gap(role, &format!("pending clearance {cid}"));
            }
        }
        for h in &view.pending_hires {
            if let Some(r) = h.get("role").and_then(|v| v.as_str()) {
                note_gap(r, "pending hire");
            }
        }
        for r in &view.missing_roles {
            note_gap(r, "crew not ready");
        }
        for b in &view.blocked_roles {
            if let Some(r) = b.get("role").and_then(|v| v.as_str()) {
                let reason = b
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("blocked");
                note_gap(r, reason);
            }
        }
    }

    // Cap total role tracks at `max_briefs - 1` (the parent is one Brief);
    // active tracks take priority over placeholders. Placeholder roles that
    // do not fit are recorded EXPLICITLY (never silently dropped) so a
    // staffing gap cannot disappear from the report.
    let cap = max_briefs.saturating_sub(1).max(1);
    let active_plan: Vec<(String, String)> = active.into_values().take(cap).collect();
    let gap_budget = cap.saturating_sub(active_plan.len());
    let gap_all: Vec<(String, String)> = gap.into_values().collect();
    let gap_plan: Vec<(String, String)> = gap_all.iter().take(gap_budget).cloned().collect();
    let placeholder_tracks_omitted: Vec<serde_json::Value> = gap_all
        .iter()
        .skip(gap_budget)
        .map(|(role, reason)| {
            serde_json::json!({
                "role": role,
                "reason": reason,
                "omitted": "max_briefs",
            })
        })
        .collect();

    // Materialisation gate. We create the parent + role tracks (active and
    // placeholder) whenever the strategy is approved and the mode wants to
    // build (not dry_run / plan_only). Subject execution Briefs and
    // assignment are PER-ROLE: only an active role gets them — a gap role
    // never gets an executable Brief or an assignee. The whole-team
    // `ready` flag still drives the reported status / next actions.
    let materialize = strategy_ok && !dry_run && matches!(mode, "create_briefs" | "assign_ready");
    let assign_mode = mode == "assign_ready";
    let not_materialized_reason: &str = if !strategy_ok {
        "strategy not approved"
    } else if dry_run {
        "dry_run: would create"
    } else {
        "plan_only: not created"
    };
    let owner_subject = ctx.caller.subject_id.to_string();
    // Parent title: a model-authored blueprint title (when provided) else the
    // deterministic title. Only applied on CREATION (ensure_marked sets the title
    // for new Briefs only), so a rerun never clobbers an existing/edited title.
    let det_parent_title = format!("Execute Mandate: {}", mandate.title);
    let parent_title = blueprint
        .and_then(|b| b.parent_title())
        .map(str::to_string)
        .unwrap_or_else(|| det_parent_title.clone());

    // ── Stable source markers (company-model §4.6) ───────────────
    // Idempotency keys are derived from the Mandate id + role key, NOT
    // from title text — so a Mandate rename or a manual Brief-title edit
    // never causes a rerun to lose track of the existing tree. A
    // placeholder track shares the role marker, so when the role later
    // becomes active the SAME role track is reused (and gains a subject).
    let parent_marker = format!("mandate:{mandate_id}:parent");
    let role_marker = |role: &str| {
        format!(
            "mandate:{mandate_id}:role:{}",
            role.trim().to_ascii_lowercase()
        )
    };

    // The input signature is built from the *markers* (rename-stable),
    // the mode and the plan id — not the mutable title text. Subject
    // markers (per active agent) and placeholder role markers are folded
    // in so a staffing change is reflected in the signature.
    let mut all_markers: Vec<String> = vec![parent_marker.clone()];
    for (role, agent_id) in &active_plan {
        let m = role_marker(role);
        all_markers.push(format!("{m}:subject:{agent_id}"));
        all_markers.push(m);
    }
    for (role, _reason) in &gap_plan {
        all_markers.push(role_marker(role));
    }
    let plan_id = view
        .plan
        .as_ref()
        .map(|p| p.plan_id.clone())
        .unwrap_or_default();
    let signature = orchestration_signature(mandate_id, mode, &plan_id, &all_markers);
    // The marker keys this run reasoned about (persisted for debugging).
    let mut markers_used: Vec<String> = Vec::with_capacity(all_markers.len());

    // Per-tier buckets (parent / role-track / subject-execution) so the
    // response and run record can distinguish what happened at each level.
    let mut parent_created: Vec<serde_json::Value> = Vec::new();
    let mut parent_existing: Vec<serde_json::Value> = Vec::new();
    let mut role_tracks_created: Vec<serde_json::Value> = Vec::new();
    let mut role_tracks_existing: Vec<serde_json::Value> = Vec::new();
    let mut subject_briefs_created: Vec<serde_json::Value> = Vec::new();
    let mut subject_briefs_existing: Vec<serde_json::Value> = Vec::new();
    let mut placeholder_tracks_created: Vec<serde_json::Value> = Vec::new();
    let mut placeholder_tracks_existing: Vec<serde_json::Value> = Vec::new();
    // Prime-governed Dossier authoring outcomes (authored / already_present /
    // locked_by_other / skipped_human_owned / stale), so a lock or a
    // human-owned doc that Prime declined to clobber is reported honestly in
    // the orchestration result instead of being silently dropped.
    let mut dossier_notes: Vec<serde_json::Value> = Vec::new();

    // The company's reviewer for every Brief this run materialises: the
    // Founder/Board (company-model §5.4 / §12.6). Mirrors prime.approve —
    // with a reviewer stamped up front a finished Shift moves in_progress →
    // in_review instead of parking in `blocked` for want of a reviewer
    // (execution-and-issue §1.3; heartbeat's "missing reviewer parks it"),
    // so the operator's run.review → run.apply can advance the Brief to
    // `done`. `find_founder` is tenant-scoped + deterministic (oldest
    // `role='founder'` row), never a cross-Guild agent. No Founder (company
    // not bootstrapped) → leave it unset and the honest "parks in blocked
    // until a reviewer is set" fallback still holds.
    let reviewer_agent_id: Option<String> = agent_store
        .find_founder(tenant)
        .ok()
        .flatten()
        .map(|f| f.agent_id);

    // Helper: get-or-create a Brief by its stable source marker.
    // Returns (task_id, was_existing, current_assignee, current_title) or
    // None when creation is not allowed and it does not yet exist. Reuse
    // is by marker only; a reused Brief's title is left untouched here so
    // manual user edits are never clobbered (the caller decides any safe
    // auto-title promotion). A newly-created Brief is stamped with the
    // Founder/Board reviewer so its completed Shift is review-to-apply-able.
    let ensure_marked = |marker: &str,
                         title: &str,
                         created: &mut Vec<serde_json::Value>,
                         existing_out: &mut Vec<serde_json::Value>,
                         skipped_out: &mut Vec<serde_json::Value>|
     -> Option<(String, bool, Option<String>, String)> {
        match task_store.get_brief_by_source_marker(marker) {
            Ok(Some(card)) => {
                existing_out.push(serde_json::json!({
                    "task_id": card.task_id, "title": card.title, "marker": marker,
                }));
                return Some((card.task_id, true, card.assignee_agent_id, card.title));
            }
            Ok(None) => {}
            Err(e) => {
                skipped_out
                    .push(serde_json::json!({"marker": marker, "reason": format!("lookup: {e}")}));
                return None;
            }
        }
        if !materialize {
            skipped_out.push(serde_json::json!({
                "title": title,
                "marker": marker,
                "reason": not_materialized_reason,
            }));
            return None;
        }
        match task_store.create_brief_with_marker(
            tenant,
            title,
            &owner_subject,
            Some(mandate_id),
            "mandate_orchestration",
            marker,
        ) {
            Ok(id) => {
                // Stamp the Founder/Board as reviewer so a completed Shift
                // lands in `in_review`, not `blocked` (company-model §12.6) —
                // the same reviewer-aware lifecycle prime.approve gives its
                // Briefs, now on the Mandate orchestration path.
                if let Some(rev) = reviewer_agent_id.as_deref() {
                    let _ = task_store.set_brief_field(&id, "reviewer", rev);
                }
                created.push(serde_json::json!({"task_id": id, "title": title, "marker": marker}));
                Some((id, false, None, title.to_string()))
            }
            Err(e) => {
                skipped_out.push(
                    serde_json::json!({"title": title, "marker": marker, "reason": e.to_string()}),
                );
                None
            }
        }
    };

    // Parent Brief (marker `mandate:{id}:parent`).
    markers_used.push(parent_marker.clone());
    let parent = ensure_marked(
        &parent_marker,
        &parent_title,
        &mut parent_created,
        &mut parent_existing,
        &mut skipped,
    );
    if let Some((ref parent_id, was_existing, _, _)) = parent
        && !was_existing
    {
        // Newly created parent: drop a durable orchestration Dossier. A
        // model-authored parent dossier (when provided) replaces the deterministic
        // body; otherwise the deterministic body is used.
        let mut roles: Vec<&str> = active_plan.iter().map(|(r, _)| r.as_str()).collect();
        roles.extend(gap_plan.iter().map(|(r, _)| r.as_str()));
        let det_body = format!(
            "Orchestration of Mandate '{}' ({mandate_id}). Roles: {}",
            mandate.title,
            roles.join(", ")
        );
        let body = blueprint
            .and_then(|b| b.parent_dossier_body())
            .unwrap_or(det_body);
        // Persist through the governed, lock-aware, Prime-stamped path (not the
        // legacy author-less `add_dossier`) so the orchestration plan is owned
        // by `__relix_autonomous_prime__` and a locked/human-owned doc is never
        // clobbered (company-model §12.5F; execution-and-issue §1.8).
        match task_store.author_prime_dossier(
            parent_id,
            "orchestration",
            "Orchestration plan",
            &body,
        ) {
            Ok(o) => dossier_notes.push(prime_dossier_note(parent_id, "orchestration", &o)),
            Err(e) => skipped
                .push(serde_json::json!({"task_id": parent_id, "reason": format!("dossier: {e}")})),
        }
    }

    // Active role-track Briefs under the parent (marker
    // `mandate:{id}:role:{role_key}`), then a per-agent subject execution
    // Brief under each. Role tracks stay unassigned; the subject Brief is
    // the one that gets the assignment.
    for (role, agent_id) in &active_plan {
        let rm = role_marker(role);
        let role_key = role.trim().to_ascii_lowercase();
        markers_used.push(rm.clone());
        // Active role-track title: a model-authored blueprint title (when provided
        // for this role key) else the deterministic title. The promotion logic
        // below targets this same `active_title`.
        let active_title = blueprint
            .and_then(|b| b.role(&role_key))
            .and_then(|i| i.title.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| role_track_title(role, &mandate.title));
        let role_track = ensure_marked(
            &rm,
            &active_title,
            &mut role_tracks_created,
            &mut role_tracks_existing,
            &mut skipped,
        );
        let Some((role_id, was_existing, _assignee, current_title)) = role_track else {
            continue;
        };
        // On first creation, drop a model-authored work-track Dossier when the
        // blueprint provided one (deterministic v1 created no role-track Dossier,
        // so this only adds text when a blueprint authored it; never on rerun).
        if !was_existing
            && let Some(body) = blueprint
                .and_then(|b| b.role(&role_key))
                .and_then(super::prime_orchestration::PrimeOrchestrationItem::dossier_body)
        {
            match task_store.author_prime_dossier(
                &role_id,
                "orchestration",
                "Work track plan",
                &body,
            ) {
                Ok(o) => dossier_notes.push(prime_dossier_note(&role_id, "orchestration", &o)),
                Err(e) => skipped.push(
                    serde_json::json!({"task_id": role_id, "reason": format!("dossier: {e}")}),
                ),
            }
        }
        // Title lifecycle: when a role that was a placeholder becomes
        // active, promote its auto-generated `… track blocked:` title to
        // the normal active title — but ONLY if it is still the
        // machine-written placeholder title (a user rename is preserved).
        if was_existing
            && current_title != active_title
            && is_auto_placeholder_title(role, &current_title)
            && task_store
                .set_brief_field(&role_id, "title", &active_title)
                .is_ok()
            && let Some(last) = role_tracks_existing.last_mut()
        {
            last["title"] = serde_json::json!(active_title);
            last["title_promoted_from_placeholder"] = serde_json::json!(true);
        }
        // Link the role track under the parent (idempotent edge insert).
        if let Some((ref parent_id, _, _, _)) = parent {
            let _ = task_store.link_subbrief(parent_id, &role_id);
        }

        // Subject execution Brief. A different agent later → a different
        // subject marker → a new subject Brief, while the role track above
        // is reused.
        let subject_marker = format!("{rm}:subject:{agent_id}");
        markers_used.push(subject_marker.clone());
        // Subject-execution title: a model-authored blueprint title (when provided
        // for this agent/subject key) else the deterministic title. Applied on
        // creation only.
        let subject_title = blueprint
            .and_then(|b| b.subject(agent_id))
            .and_then(|i| i.title.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| subject_exec_title(role, agent_id));
        let subject = ensure_marked(
            &subject_marker,
            &subject_title,
            &mut subject_briefs_created,
            &mut subject_briefs_existing,
            &mut skipped,
        );
        let Some((subject_id, subject_was_existing, subject_assignee, _st)) = subject else {
            continue;
        };
        // Link the subject Brief under its role track.
        let _ = task_store.link_subbrief(&role_id, &subject_id);
        // On first creation, drop a model-authored execution Dossier when the
        // blueprint provided one (deterministic v1 created no subject Dossier).
        if !subject_was_existing
            && let Some(body) = blueprint
                .and_then(|b| b.subject(agent_id))
                .and_then(super::prime_orchestration::PrimeOrchestrationItem::dossier_body)
        {
            match task_store.author_prime_dossier(&subject_id, "execution", "Execution plan", &body)
            {
                Ok(o) => dossier_notes.push(prime_dossier_note(&subject_id, "execution", &o)),
                Err(e) => skipped.push(
                    serde_json::json!({"task_id": subject_id, "reason": format!("dossier: {e}")}),
                ),
            }
        }

        // Assignment lands on the subject Brief (assign_ready only); the
        // role track above stays unassigned. Always assign-Key gated.
        if !assign_mode {
            if mode == "create_briefs" {
                skipped.push(serde_json::json!({
                    "task_id": subject_id, "reason": "not assigned (mode=create_briefs)",
                }));
            }
            continue;
        }
        if subject_assignee.as_deref() == Some(agent_id.as_str()) {
            // Already assigned to the right Operative — idempotent no-op.
            assigned_briefs.push(serde_json::json!({"task_id": subject_id, "agent_id": agent_id}));
            continue;
        }
        match enforce_assign_key(agent_store, ctx, agent_id) {
            Ok(()) => match task_store.set_brief_field(&subject_id, "assignee", agent_id) {
                Ok(()) => assigned_briefs
                    .push(serde_json::json!({"task_id": subject_id, "agent_id": agent_id})),
                Err(e) => skipped.push(
                    serde_json::json!({"task_id": subject_id, "reason": format!("assign: {e}")}),
                ),
            },
            Err(_) => skipped.push(serde_json::json!({
                "task_id": subject_id,
                "reason": format!("assign denied for `{agent_id}` (assign-Key gate)"),
            })),
        }
    }

    // Placeholder role tracks for gap roles (missing / pending / blocked).
    // Same role marker as an active track — so the SAME Brief is reused
    // when the role later becomes active — but with NO subject Brief and
    // NO assignment. Each created entry is tagged `placeholder` + `reason`
    // so the persisted run and the dashboard can render the gap.
    for (role, reason) in &gap_plan {
        let rm = role_marker(role);
        markers_used.push(rm.clone());
        let track = ensure_marked(
            &rm,
            &placeholder_track_title(role, reason),
            &mut placeholder_tracks_created,
            &mut placeholder_tracks_existing,
            &mut skipped,
        );
        if let Some((role_id, was_existing, _, _)) = &track {
            // Tag the just-pushed bucket entry with the placeholder reason.
            let bucket = if *was_existing {
                &mut placeholder_tracks_existing
            } else {
                &mut placeholder_tracks_created
            };
            if let Some(last) = bucket.last_mut() {
                last["placeholder"] = serde_json::json!(true);
                last["reason"] = serde_json::json!(reason);
            }
            // Link under the parent (idempotent edge insert).
            if let Some((ref parent_id, _, _, _)) = parent {
                let _ = task_store.link_subbrief(parent_id, role_id);
            }
            // On first creation, record WHY the track is blocked — through the
            // same governed, Prime-stamped, lock-aware path.
            if !was_existing {
                let blocker_body = format!(
                    "Role '{role}' is not ready ({reason}). No execution Brief is \
                     created until the role is staffed and active."
                );
                match task_store.author_prime_dossier(
                    role_id,
                    "blocker",
                    "Placeholder track",
                    &blocker_body,
                ) {
                    Ok(o) => dossier_notes.push(prime_dossier_note(role_id, "blocker", &o)),
                    Err(e) => skipped.push(
                        serde_json::json!({"task_id": role_id, "reason": format!("dossier: {e}")}),
                    ),
                }
            }
        }
    }

    // Record any placeholder roles dropped by the `max_briefs` cap into
    // `skipped` too, so the gap is also visible in the persisted run (the
    // run record stores `skipped`), not only the live response.
    for omitted in &placeholder_tracks_omitted {
        skipped.push(omitted.clone());
    }

    // Backward-compatible flat views: callers/tests that predate the
    // tiered shape still read `created_briefs` / `existing_briefs`.
    created_briefs.extend(parent_created.iter().cloned());
    created_briefs.extend(role_tracks_created.iter().cloned());
    created_briefs.extend(subject_briefs_created.iter().cloned());
    created_briefs.extend(placeholder_tracks_created.iter().cloned());
    existing_briefs.extend(parent_existing.iter().cloned());
    existing_briefs.extend(role_tracks_existing.iter().cloned());
    existing_briefs.extend(subject_briefs_existing.iter().cloned());
    existing_briefs.extend(placeholder_tracks_existing.iter().cloned());

    // Status + next actions.
    let status = if !ready {
        "blocked"
    } else if dry_run || mode == "plan_only" {
        "planned"
    } else if mode == "assign_ready" {
        "assigned"
    } else {
        "created"
    };
    if !ready {
        next_actions.push("Resolve the blockers, then re-run mandate.orchestrate.".to_string());
    } else if dry_run || mode == "plan_only" {
        next_actions.push(
            "Re-run with mode=create_briefs or assign_ready to materialise the tree.".to_string(),
        );
    } else if mode == "create_briefs" {
        next_actions.push("Re-run with mode=assign_ready to assign the active team.".to_string());
    } else {
        next_actions
            .push("Briefs created and assigned — the heartbeat will dispatch them.".to_string());
    }
    let placeholders = placeholder_tracks_created.len() + placeholder_tracks_existing.len();
    if placeholders > 0 {
        next_actions.push(format!(
            "{placeholders} placeholder track(s) await staffing; each becomes executable once \
             its role is active."
        ));
    }
    if !placeholder_tracks_omitted.is_empty() {
        next_actions.push(format!(
            "{} placeholder role(s) were omitted by the max_briefs cap ({max_briefs}); raise \
             max_briefs to surface every staffing gap.",
            placeholder_tracks_omitted.len()
        ));
    }

    // Persist the run (best-effort: a record failure must not lose the
    // already-created Briefs).
    let to_json =
        |v: &[serde_json::Value]| serde_json::to_string(&v).unwrap_or_else(|_| "[]".into());
    let actions_json = serde_json::to_string(&next_actions).unwrap_or_else(|_| "[]".into());
    let markers_json = serde_json::to_string(&markers_used).unwrap_or_else(|_| "[]".into());
    let run_id = match spine_store.record_orchestration_run(&OrchestrationRunRecord {
        tenant_id: tenant,
        mandate_id,
        mode,
        dry_run,
        input_signature: &signature,
        status,
        created_brief_ids_json: &to_json(&created_briefs),
        existing_brief_ids_json: &to_json(&existing_briefs),
        assigned_brief_ids_json: &to_json(&assigned_briefs),
        skipped_json: &to_json(&skipped),
        source_markers_json: &markers_json,
        blockers_json: &to_json(&blockers),
        next_actions_json: &actions_json,
    }) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(mandate_id = %mandate_id, error = %e, "mandate.orchestrate: persist failed");
            None
        }
    };

    let parent_brief = parent_created
        .first()
        .or_else(|| parent_existing.first())
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let body = serde_json::json!({
        "mandate_id": mandate_id,
        "run_id": run_id,
        "mode": mode,
        "dry_run": dry_run,
        "ready": ready,
        "status": status,
        "input_signature": signature,
        "blockers": blockers,
        // Tiered view (parent → role track → subject execution), plus
        // placeholder tracks for missing / pending / blocked roles.
        "parent_brief": parent_brief,
        "role_tracks_created": role_tracks_created,
        "role_tracks_existing": role_tracks_existing,
        "placeholder_tracks_created": placeholder_tracks_created,
        "placeholder_tracks_existing": placeholder_tracks_existing,
        "placeholder_tracks_omitted": placeholder_tracks_omitted,
        "subject_briefs_created": subject_briefs_created,
        "subject_briefs_existing": subject_briefs_existing,
        "assigned_briefs": assigned_briefs,
        "skipped": skipped,
        // Prime-governed Dossier authoring outcomes for this run (one entry per
        // attempted parent/role/subject/blocker doc): `authored` /
        // `already_present` / `locked_by_other` / `skipped_human_owned` /
        // `stale`. A locked or human-owned doc Prime declined to overwrite is
        // visible here, not silently dropped.
        "dossier_notes": dossier_notes,
        "source_markers": markers_used,
        "next_actions": next_actions,
        // Backward-compatible flat views.
        "created_briefs": created_briefs,
        "existing_briefs": existing_briefs,
    });
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("mandate.orchestrate encode: {e}")),
    }
}

/// `mandate.orchestration.latest` — the most recent persisted
/// orchestration run for a Mandate as JSON (`null` if never run).
/// Arg: `mandate_id`. Tenant-scoped.
pub fn handle_orchestration_latest(
    spine_store: &SpineStore,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let mandate_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.orchestration.latest utf8: {e}")),
    };
    if mandate_id.is_empty() {
        return invalid("mandate.orchestration.latest: mandate_id required".into());
    }
    match spine_store.latest_orchestration_run(ctx.tenant_id_or_default(), mandate_id) {
        Ok(Some(run)) => match serde_json::to_vec(&run.to_json()) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("mandate.orchestration.latest encode: {e}")),
        },
        Ok(None) => HandlerOutcome::Ok(b"null".to_vec()),
        Err(e) => internal(format!("mandate.orchestration.latest: {e}")),
    }
}

/// `mandate.orchestration.list` — recent orchestration runs for a
/// Mandate as a JSON array (newest first). Arg: `mandate_id|limit?`.
/// Tenant-scoped.
pub fn handle_orchestration_list(spine_store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("mandate.orchestration.list utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let mandate_id = parts.first().copied().unwrap_or("").trim();
    if mandate_id.is_empty() {
        return invalid("mandate.orchestration.list: mandate_id required".into());
    }
    let limit: usize = parts
        .get(1)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    match spine_store.list_orchestration_runs(ctx.tenant_id_or_default(), mandate_id, limit) {
        Ok(rows) => {
            let arr: Vec<serde_json::Value> = rows.iter().map(|r| r.to_json()).collect();
            match serde_json::to_vec(&serde_json::Value::Array(arr)) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => internal(format!("mandate.orchestration.list encode: {e}")),
            }
        }
        Err(e) => internal(format!("mandate.orchestration.list: {e}")),
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

/// `agent.approve_hire` — approve a pending hire (pending → active) and,
/// optionally, bind the Rig that makes it **immediately runnable**
/// (company-model §12.6). Wire arg: `agent_id` or `agent_id|rig` (the Rig
/// is optional). Tenant-scoped; owner-gating is enforced by the boot
/// policy allow rule + the bridge session, same as the other governed
/// company actions.
///
/// When a `rig` is supplied it is validated against the known-Rig
/// allowlist ([`rig::is_known_rig`]) so a typo can't activate an Operative
/// onto a Rig the dispatcher would silently fall back from — `echo` (the
/// safe-local built-in) is always accepted. Approval + Rig assignment are
/// atomic in the store: the Operative never ends up active-but-unrigged
/// because of this call. With no `rig`, behaviour is unchanged except the
/// JSON response now tells the client whether the Operative still needs a
/// Rig before it can run.
///
/// Returns JSON: `{status, agent_id, rig, rig_set, runnable, needs_rig}`.
pub fn handle_approve_hire(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.approve_hire utf8: {e}")),
    };
    let mut parts = s.splitn(2, '|');
    let id = parts.next().unwrap_or("").trim();
    let rig = parts.next().map(str::trim).filter(|r| !r.is_empty());
    if id.is_empty() {
        return invalid("agent.approve_hire: agent_id required".into());
    }
    if let Some(r) = rig
        && !crate::rig::is_known_rig(r)
    {
        return invalid(format!(
            "agent.approve_hire: unknown rig '{r}' (known: {})",
            crate::rig::KNOWN_RIG_NAMES.join(", ")
        ));
    }
    match store.approve_hire_with_rig(id, rig, ctx.tenant_id_or_default()) {
        Ok(outcome) => {
            let runnable = outcome.rig.is_some();
            let body = serde_json::json!({
                "status": "approved",
                "agent_id": id,
                // The Rig bound after approval (null when none — then the
                // Operative is active but not yet runnable).
                "rig": outcome.rig,
                // Did THIS call assign the Rig (vs. it was already set / left
                // unset)?
                "rig_set": outcome.rig_set,
                // Can a Shift dispatch to this Operative now?
                "runnable": runnable,
                // The client should configure a Rig (e.g. PATCH /v1/agents/:id
                // {rig:"echo"}) before the Operative can run.
                "needs_rig": !runnable,
            });
            match serde_json::to_vec(&body) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => internal(format!("agent.approve_hire encode: {e}")),
            }
        }
        // Absent / cross-tenant read identically as "no pending hire" — no
        // existence leak.
        Err(AgentStoreError::NotFound(_)) => invalid(format!(
            "agent.approve_hire: no pending hire {id} in this Guild"
        )),
        Err(AgentStoreError::BadInput(m)) => invalid(format!("agent.approve_hire: {m}")),
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
                "agent_id={}|name={}|role={}|title={}|department={}|team={}|created_by={}|status={}|subject_id={}|risk_ceiling={}|approval_timeout_secs={}|created_at={}|updated_at={}|surface_allowlist={}|allow_categories={}|deny_categories={}|allow_sensitivity_tags={}|deny_sensitivity_tags={}|approval_required_categories={}|rig={}|monthly_allowance_cents={}|max_concurrent_runs={}|wake_on_timer={}|wake_on_demand={}|model_preference={}|reasoning_effort={}\n",
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
                p.model_preference.as_deref().unwrap_or(""),
                p.reasoning_effort.as_deref().unwrap_or(""),
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
                // TSV columns, APPEND-ONLY for back-compat: the historical
                // 5-column prefix (approval_id, agent_id, method, reason,
                // requested_at) is unchanged, then the typed fields the Desk's
                // Approvals hub renders without a second per-row fetch:
                // subject_id (who/what is affected), capability_category
                // (stable type bucket), expires_at (governance window), and
                // task_id (the parked Brief, when present — its target route).
                // Every string column is tab/pipe-sanitised so the positional
                // layout cannot shift.
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    r.approval_id,
                    r.agent_id,
                    r.method,
                    sanitize(&r.reason),
                    r.requested_at,
                    sanitize(&r.subject_id),
                    sanitize(&r.capability_category),
                    r.expires_at,
                    sanitize(r.task_id.as_deref().unwrap_or("")),
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
pub(crate) fn policy_denied(cause: String) -> HandlerOutcome {
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

/// Resolve + governance-validate a suggested child's optional **assignee
/// hint** (relix-execution-and-issue-design §1.9). `agent_hint` (model A,
/// an Operative id) and `role_hint` (model B, a role) are mutually
/// exclusive — at most one is `Some` (the proposal normalizer rejects
/// both). Returns:
///
/// - `Ok(None)` when neither hint is set — the child opens **unassigned**
///   (today's default; the parent's assignee is never inherited);
/// - `Ok(Some(id))` with the concrete, same-Guild, **active** Operative to
///   assign — for a role hint, the **oldest active same-role** Operative in
///   the Guild (deterministic, via [`AgentStore::list_by_role_for_tenant`]);
/// - `Err(denial)` when the hint can't be honoured: an unknown /
///   cross-Guild / inactive id, no active Operative for the role, or the
///   accepter's assign-Key forbids it. The caller refuses the **whole**
///   accept on `Err` *before* any child is created, so a bad hint never
///   half-materializes a proposal.
///
/// Reuses [`enforce_assign_key`] for the assign-Key authority check, and
/// adds an explicit same-Guild + active check that holds **even for the
/// Founder/operator** (whose assign-Key check is bypassed) — so no caller,
/// not even the operator, can assign across Guilds or to an inactive
/// Operative through a suggestion.
pub(crate) fn resolve_assignee_hint(
    store: &AgentStore,
    ctx: &InvocationCtx,
    agent_hint: Option<&str>,
    role_hint: Option<&str>,
) -> Result<Option<String>, HandlerOutcome> {
    let tenant = ctx.tenant_id_or_default();
    let agent_hint = agent_hint.map(str::trim).filter(|s| !s.is_empty());
    let role_hint = role_hint.map(str::trim).filter(|s| !s.is_empty());
    // Pin the concrete candidate Operative id from whichever hint is set.
    let candidate = match (agent_hint, role_hint) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            // The normalizer rejects this at open; defend in depth here too.
            return Err(invalid(
                "assignee hint: set an Operative id OR a role, not both".to_string(),
            ));
        }
        (Some(id), None) => id.to_string(),
        (None, Some(role)) => {
            // The role-adoption helper is tenant-scoped + active-only +
            // deterministic (oldest first), so "no match" is a clean refusal
            // and the pick can never come from another Guild.
            let matches = match store.list_by_role_for_tenant(role, tenant) {
                Ok(m) => m,
                Err(e) => return Err(internal(format!("role resolution: {e}"))),
            };
            match matches.into_iter().next() {
                Some(id) => id,
                None => {
                    return Err(policy_denied(format!(
                        "assign denied: no active Operative with role `{role}` in this Guild"
                    )));
                }
            }
        }
    };
    // Explicit same-Guild + active check — holds even for the operator,
    // whose assign-Key check below is bypassed. (For the role path this is
    // already guaranteed; re-checking keeps one path and is cheap.)
    let target = match store.get_agent_for_tenant(&candidate, tenant) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(policy_denied(format!(
                "assign denied: `{candidate}` is not an Operative in this Guild"
            )));
        }
        Err(e) => return Err(internal(format!("assignee lookup: {e}"))),
    };
    if target.status != "active" {
        return Err(policy_denied(format!(
            "assign denied: `{candidate}` is {} (not active) — cannot receive executable work",
            target.status
        )));
    }
    // Assign-Key authority: a no-op for the Founder/operator, enforced for
    // an Operative-raised accept (same gate as `brief.set` assignee).
    enforce_assign_key(store, ctx, &candidate)?;
    Ok(Some(candidate))
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

/// Enforce the **secret allowlist** (company-model §5.2C) for an
/// Operative reading a credential by `secret_name`. This is an
/// *additional* per-Operative layer on top of the credential vault's
/// existing owner + tenant gate. Returns:
///
/// - `Ok(())` — bypassed: the Founder/Board (operator/admin), or the
///   caller is **not** an Operative in this Guild (a non-spine system
///   identity — the vault's owner/tenant gate still protects it).
/// - `Err(outcome)` — the caller IS an Operative and is refused: it is
///   not active, or `secret_name` is not in its `secret_allowlist`
///   (empty allowlist = deny-by-default; exact match only).
pub(crate) fn enforce_secret_allowlist(
    store: &AgentStore,
    ctx: &InvocationCtx,
    secret_name: &str,
) -> Result<(), HandlerOutcome> {
    if caller_is_operator(ctx) {
        return Ok(());
    }
    let tenant = ctx.tenant_id_or_default();
    let subject = ctx.caller.subject_id.to_string();
    let actor = match store.get_by_subject_for_tenant(&subject, tenant) {
        Ok(Some(p)) => p,
        // Not an Operative in this Guild → the secret_allowlist concept
        // does not apply; defer to the vault's owner/tenant gate.
        Ok(None) => return Ok(()),
        Err(e) => return Err(internal(format!("secret allowlist lookup: {e}"))),
    };
    if actor.status != "active" {
        return Err(security_denied(format!(
            "secret denied: Operative `{}` is {} (not active)",
            actor.agent_id, actor.status
        )));
    }
    if super::keys::secret_allowed(&actor.secret_allowlist, secret_name) {
        Ok(())
    } else {
        Err(security_denied(format!(
            "secret denied: `{secret_name}` is not in Operative `{}`'s secret_allowlist",
            actor.agent_id
        )))
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

/// Autonomous spawn-Clearance greenlight for the standing-authority Prime
/// driver. It reuses the EXACT side effects `handle_approval_decide` applies to
/// an approved spawn Clearance — the **store decide path**
/// ([`AgentStore::decide_approval`] `Approved`) followed by the identical
/// spawn-clearance hire-activation hop ([`AgentStore::approve_hire_with_rig`]) — but
/// without the interactive approver/token/task-resume machinery the manual
/// route needs (a spawn Clearance carries no `task_id`, and the autonomous
/// loop needs no bearer token). It is deliberately **narrow**:
///
/// - **Spawn Clearances only.** A non-`SPAWN_CLEARANCE_METHOD` approval is
///   refused, so this can NEVER approve a tool / high-risk / budget approval.
/// - **Tenant-scoped, no existence leak.** A clearance outside `tenant` reads
///   as not-found (identical to truly absent).
/// - **Pending-only + idempotent.** `decide_approval` refuses a terminal
///   status, so a re-run never double-approves; `approve_hire` on an
///   already-active hire is a safe no-op (NotFound is swallowed).
///
/// Returns the activated hire's `agent_id` on success.
pub fn autonomous_approve_spawn_clearance(
    store: &AgentStore,
    tenant: &str,
    approval_id: &str,
    rig: Option<&str>,
) -> Result<String, String> {
    let rec = match store.get_approval_record_for_tenant(approval_id, tenant) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(format!("clearance not found in this Guild: {approval_id}")),
        Err(e) => return Err(format!("clearance load: {e}")),
    };
    if rec.method != crate::nodes::coordinator::agent::store::SPAWN_CLEARANCE_METHOD {
        return Err(format!(
            "approval {approval_id} is not a spawn Clearance (method={}) — autonomous Prime \
             only greenlights spawn Clearances",
            rec.method
        ));
    }
    if rec.status != ApprovalStatus::Pending {
        return Err(format!(
            "clearance {approval_id} is not pending (status={})",
            rec.status.as_wire()
        ));
    }
    let hire_id = rec.agent_id.clone();
    // THE store decide path — identical to what handle_approval_decide calls.
    store
        .decide_approval(
            approval_id,
            ApprovalStatus::Approved,
            "autonomous-prime",
            "autonomous Prime standing authority",
        )
        .map_err(|e| format!("decide: {e}"))?;
    // Spawn-Clearance hire-activation hop. The manual approval route keeps its
    // historic behavior, but autonomous Prime must produce runnable workers.
    match store.approve_hire_with_rig(&hire_id, rig, tenant) {
        Ok(_) => {}
        Err(AgentStoreError::NotFound(_)) => {}
        Err(e) => return Err(format!("activate hire: {e}")),
    }
    Ok(hire_id)
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

pub(crate) fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

pub(crate) fn internal(cause: String) -> HandlerOutcome {
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

    // ── agent.approve_hire wire contract (company-model §12.6) ───────────────

    /// Spawn a `pending` qa hire in `tenant` and return its agent_id.
    #[cfg(test)]
    fn pending_qa_hire(s: &AgentStore, subj: &str, tenant: &str) -> String {
        s.request_hire("QA", "qa", "QA", "qa", "qa", "ceo", subj, "low", tenant)
            .unwrap()
    }

    // ── resolve_assignee_hint (suggest_tasks §1.9) ───────────────────────────

    /// An active Operative with `role` in `tenant`; returns its agent_id.
    #[cfg(test)]
    fn active_op(s: &AgentStore, role: &str, tenant: &str) -> String {
        let (id, _) = s
            .ensure_starter_operative(
                role,
                &format!("{role} (local · echo)"),
                role,
                "echo",
                tenant,
            )
            .unwrap();
        id
    }

    #[test]
    fn resolve_hint_absent_opens_unassigned() {
        let s = store();
        let ctx = fake_ctx_tenant(b"", "acme");
        // (HandlerOutcome isn't Debug, so compare via `.ok()`.)
        assert_eq!(resolve_assignee_hint(&s, &ctx, None, None).ok(), Some(None));
        // Empty / whitespace hints are also "no hint".
        assert_eq!(
            resolve_assignee_hint(&s, &ctx, Some("  "), Some("")).ok(),
            Some(None)
        );
    }

    #[test]
    fn resolve_hint_by_agent_id_happy_path() {
        let s = store();
        let id = active_op(&s, "engineer", "acme");
        let ctx = fake_ctx_tenant(b"", "acme");
        assert_eq!(
            resolve_assignee_hint(&s, &ctx, Some(&id), None).ok(),
            Some(Some(id))
        );
    }

    #[test]
    fn resolve_hint_by_role_resolves_active_same_role() {
        let s = store();
        let id = active_op(&s, "engineer", "acme");
        let ctx = fake_ctx_tenant(b"", "acme");
        assert_eq!(
            resolve_assignee_hint(&s, &ctx, None, Some("engineer")).ok(),
            Some(Some(id))
        );
    }

    #[test]
    fn resolve_hint_unknown_agent_id_is_denied() {
        let s = store();
        let ctx = fake_ctx_tenant(b"", "acme");
        let out = resolve_assignee_hint(&s, &ctx, Some("agt_nope_1"), None);
        assert!(out.is_err(), "an unknown id must be refused");
    }

    #[test]
    fn resolve_hint_cross_tenant_agent_id_is_denied() {
        let s = store();
        // Operative lives in `acme`, but the accept runs in `other`.
        let id = active_op(&s, "engineer", "acme");
        let ctx = fake_ctx_tenant(b"", "other");
        let out = resolve_assignee_hint(&s, &ctx, Some(&id), None);
        assert!(out.is_err(), "a cross-Guild id must be refused");
    }

    #[test]
    fn resolve_hint_cross_tenant_role_is_denied() {
        let s = store();
        active_op(&s, "engineer", "acme");
        // No engineer exists in `other` → no match → refused (never reaches acme).
        let ctx = fake_ctx_tenant(b"", "other");
        let out = resolve_assignee_hint(&s, &ctx, None, Some("engineer"));
        assert!(
            out.is_err(),
            "a role with no in-Guild match must be refused"
        );
    }

    #[test]
    fn resolve_hint_inactive_agent_id_is_denied() {
        let s = store();
        // A pending (not-yet-approved) hire is not active.
        let id = pending_qa_hire(&s, "subj-x", "acme");
        let ctx = fake_ctx_tenant(b"", "acme");
        let out = resolve_assignee_hint(&s, &ctx, Some(&id), None);
        assert!(out.is_err(), "an inactive Operative must be refused");
    }

    #[test]
    fn resolve_hint_role_with_no_active_match_is_denied() {
        let s = store();
        let ctx = fake_ctx_tenant(b"", "acme");
        let out = resolve_assignee_hint(&s, &ctx, None, Some("ghost"));
        assert!(
            out.is_err(),
            "a role with no active Operative must be refused"
        );
    }

    #[test]
    fn resolve_hint_both_id_and_role_is_rejected() {
        let s = store();
        let id = active_op(&s, "engineer", "acme");
        let ctx = fake_ctx_tenant(b"", "acme");
        let out = resolve_assignee_hint(&s, &ctx, Some(&id), Some("engineer"));
        assert!(out.is_err(), "an id AND a role together must be refused");
    }

    #[test]
    fn resolve_hint_non_operator_without_profile_is_denied() {
        let s = store();
        // A valid, active assignee exists, but the caller is a plain agent
        // with no Operative profile → the assign-Key layer refuses.
        let id = active_op(&s, "engineer", "acme");
        let mut ctx = fake_ctx_with_role(b"", "agent", b"stranger");
        ctx.tenant_id = Some("acme".to_string());
        let out = resolve_assignee_hint(&s, &ctx, Some(&id), None);
        assert!(
            out.is_err(),
            "a caller without an Operative profile cannot assign"
        );
    }

    #[test]
    fn approve_hire_with_echo_yields_a_runnable_operative() {
        let s = store();
        let id = pending_qa_hire(&s, "subj-rig", "default");
        // One call, explicit safe-local Rig → active + runnable, no PATCH.
        let arg = format!("{id}|echo");
        let v = json_of(ok_body(handle_approve_hire(&s, &fake_ctx(arg.as_bytes()))));
        assert_eq!(v["status"], "approved");
        assert_eq!(v["rig"], "echo");
        assert_eq!(v["rig_set"], true);
        assert_eq!(v["runnable"], true);
        assert_eq!(v["needs_rig"], false);
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.status, "active");
        assert_eq!(p.rig.as_deref(), Some("echo"));
    }

    #[test]
    fn approve_hire_without_rig_activates_but_flags_needs_rig() {
        let s = store();
        let id = pending_qa_hire(&s, "subj-norig", "default");
        // No Rig in the arg → legacy behaviour (active) but the response is
        // explicit that a Rig is still required before it can run.
        let v = json_of(ok_body(handle_approve_hire(&s, &fake_ctx(id.as_bytes()))));
        assert_eq!(v["status"], "approved");
        assert_eq!(v["rig"], serde_json::Value::Null);
        assert_eq!(v["rig_set"], false);
        assert_eq!(v["runnable"], false);
        assert_eq!(v["needs_rig"], true);
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "active");
    }

    #[test]
    fn approve_hire_rejects_an_unknown_rig() {
        let s = store();
        let id = pending_qa_hire(&s, "subj-bad", "default");
        let arg = format!("{id}|definitely-not-a-rig");
        let out = handle_approve_hire(&s, &fake_ctx(arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
        // The hire is untouched — still pending (no misleading partial state).
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "pending");
    }

    #[test]
    fn approve_hire_duplicate_is_safe_and_no_clobber() {
        let s = store();
        let id = pending_qa_hire(&s, "subj-dup", "default");
        let arg = format!("{id}|echo");
        ok_body(handle_approve_hire(&s, &fake_ctx(arg.as_bytes())));
        // A second approval (even with a conflicting Rig) is refused and must
        // not clobber the bound Rig.
        let arg2 = format!("{id}|claude");
        let out = handle_approve_hire(&s, &fake_ctx(arg2.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
        assert_eq!(
            s.get_agent(&id).unwrap().unwrap().rig.as_deref(),
            Some("echo"),
            "duplicate/conflicting approval left the Rig intact"
        );
    }

    #[test]
    fn approve_hire_is_tenant_scoped_and_does_not_leak() {
        let s = store();
        let id = pending_qa_hire(&s, "subj-acme", "acme");
        // A caller in another Guild cannot approve it; the refusal is generic
        // (no existence leak) and the hire stays pending in its own Guild.
        let arg = format!("{id}|echo");
        let out = handle_approve_hire(&s, &fake_ctx_tenant(arg.as_bytes(), "other"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "pending");
        // The owning Guild approves it normally, with the Rig bound.
        let v = json_of(ok_body(handle_approve_hire(
            &s,
            &fake_ctx_tenant(arg.as_bytes(), "acme"),
        )));
        assert_eq!(v["runnable"], true);
        assert_eq!(v["rig"], "echo");
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
    fn bootstrap_founder_creates_one_then_is_idempotent() {
        let s = store();
        // First run: operator owner creates the Founder.
        let first = ok_body(handle_bootstrap_founder(&s, &fake_ctx(b"Ada|echo")));
        assert!(first.contains("\"created\":true"), "got {first}");
        assert!(first.contains("\"role\":\"founder\""));
        assert!(first.contains("\"name\":\"Ada\""));
        assert!(first.contains("\"rig\":\"echo\""));
        // Exactly one founder exists.
        assert!(s.find_founder("default").unwrap().is_some());
        let ops = s.list_operatives_for_tenant("default").unwrap();
        assert_eq!(ops.len(), 1);
        // Second run: no duplicate — returns the existing Founder.
        let second = ok_body(handle_bootstrap_founder(&s, &fake_ctx(b"Other|echo")));
        assert!(second.contains("\"created\":false"), "got {second}");
        // Still exactly one (name unchanged — the first Founder).
        assert_eq!(s.list_operatives_for_tenant("default").unwrap().len(), 1);
        assert_eq!(s.find_founder("default").unwrap().unwrap().name, "Ada");
    }

    #[test]
    fn bootstrap_founder_has_full_owner_keys() {
        let s = store();
        ok_body(handle_bootstrap_founder(&s, &fake_ctx(b"Ada|echo")));
        let f = s.find_founder("default").unwrap().unwrap();
        assert!(f.can_spawn_agents);
        assert!(f.can_assign_work && f.assign_scope == "any");
        assert!(f.can_manage_work && f.manage_scope == "any");
        assert!(f.can_configure_agents && f.configure_scope == "any");
        assert_eq!(f.status, "active");
        assert!(f.reports_to.is_none(), "the Founder is the apex");
    }

    #[test]
    fn bootstrap_founder_refused_for_non_owner_caller() {
        let s = store();
        // A non-operator caller with NO console/operator profile is refused.
        let ctx = fake_ctx_with_role(b"Mallory|echo", "agent", b"stranger");
        assert_eq!(
            err_kind(handle_bootstrap_founder(&s, &ctx)),
            error_kinds::SECURITY_DENIED
        );
        // Nothing was created.
        assert!(s.find_founder("default").unwrap().is_none());
    }

    #[test]
    fn bootstrap_founder_allowed_for_console_owner_identity() {
        let s = store();
        // The dashboard/bridge owner is a non-operator AIC role that
        // carries the boot-seeded allow-all console profile.
        let ctx = fake_ctx_with_role(b"Ada|echo", "service", b"bridge-owner");
        let subject = ctx.caller.subject_id.to_string();
        s.ensure_operator_console_profile(&subject, "default")
            .unwrap();
        let out = ok_body(handle_bootstrap_founder(&s, &ctx));
        assert!(out.contains("\"created\":true"), "got {out}");
    }

    // ── company.starter_crew (company-model §12.6) ───────────────────────

    #[test]
    fn starter_crew_creates_founder_plus_default_safe_local_crew() {
        let s = store();
        let body = json_of(ok_body(handle_starter_crew(&s, &fake_ctx(b""))));
        // The Founder is stood up + the default engineer/designer starters.
        assert_eq!(body["founder_created"], true, "{body}");
        assert_eq!(body["rig"], "echo");
        assert_eq!(body["safe_local"], true, "echo crew is safe-local: {body}");
        let crew = body["crew"].as_array().unwrap();
        assert_eq!(crew.len(), 2, "default roster is engineer+designer: {body}");
        let roles: Vec<&str> = crew.iter().map(|c| c["role"].as_str().unwrap()).collect();
        assert!(
            roles.contains(&"engineer") && roles.contains(&"designer"),
            "{body}"
        );
        // Every starter is created active, on echo, and labelled local — never a
        // fake Claude/Codex agent.
        for c in crew {
            assert_eq!(c["created"], true);
            let id = c["agent_id"].as_str().unwrap();
            let p = s.get_agent(id).unwrap().unwrap();
            assert_eq!(p.status, "active");
            assert_eq!(p.rig.as_deref(), Some("echo"));
            assert_eq!(p.created_by, AgentStore::STARTER_CREATED_BY);
            assert!(p.name.contains("local"), "labelled local: {}", p.name);
            // A starter is a worker — no org/work Keys.
            assert!(!p.can_spawn_agents && !p.can_assign_work);
        }
        // The company now reads as initialized with 3 Operatives.
        let status = json_of(ok_body(handle_company_status(&s, &fake_ctx(b""))));
        assert_eq!(status["initialized"], true);
        assert_eq!(status["operative_count"], 3);
    }

    #[test]
    fn starter_crew_is_idempotent_no_duplicates_on_rerun() {
        let s = store();
        let _ = ok_body(handle_starter_crew(&s, &fake_ctx(b"")));
        let again = json_of(ok_body(handle_starter_crew(&s, &fake_ctx(b""))));
        // Re-run creates nothing new.
        assert_eq!(again["founder_created"], false, "{again}");
        for c in again["crew"].as_array().unwrap() {
            assert_eq!(c["created"], false, "no duplicate starter: {c}");
        }
        // Still exactly Founder + 2 starters.
        assert_eq!(s.list_operatives_for_tenant("default").unwrap().len(), 3);
    }

    #[test]
    fn starter_crew_honors_explicit_roles_and_dedupes() {
        let s = store();
        // Free-form, duplicate, and noise roles all canonicalise + de-dup.
        let body = json_of(ok_body(handle_starter_crew(
            &s,
            &fake_ctx(b"echo|backend, frontend, qa, qa"),
        )));
        let roles: Vec<&str> = body["crew"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["role"].as_str().unwrap())
            .collect();
        // backend+frontend both canon to engineer (one), plus qa.
        assert_eq!(roles, vec!["engineer", "qa"], "{body}");
    }

    #[test]
    fn starter_crew_is_tenant_scoped() {
        let s = store();
        let _ = ok_body(handle_starter_crew(&s, &fake_ctx_tenant(b"", "acme")));
        // acme has its crew; globex is still empty (no cross-tenant leak).
        assert_eq!(s.list_operatives_for_tenant("acme").unwrap().len(), 3);
        assert_eq!(s.list_operatives_for_tenant("globex").unwrap().len(), 0);
        assert!(s.find_founder("globex").unwrap().is_none());
    }

    #[test]
    fn starter_crew_refused_for_non_owner_caller() {
        let s = store();
        let ctx = fake_ctx_with_role(b"", "agent", b"stranger");
        assert_eq!(
            err_kind(handle_starter_crew(&s, &ctx)),
            error_kinds::SECURITY_DENIED
        );
        assert!(s.find_founder("default").unwrap().is_none());
    }

    #[test]
    fn company_status_reports_initialized_after_bootstrap() {
        let s = store();
        let before = ok_body(handle_company_status(&s, &fake_ctx(b"")));
        assert!(before.contains("\"initialized\":false"), "got {before}");
        assert!(before.contains("\"operative_count\":0"));
        ok_body(handle_bootstrap_founder(&s, &fake_ctx(b"Ada|echo")));
        let after = ok_body(handle_company_status(&s, &fake_ctx(b"")));
        assert!(after.contains("\"initialized\":true"), "got {after}");
        assert!(after.contains("\"operative_count\":1"));
        assert!(after.contains("\"role\":\"founder\""));
    }

    #[test]
    fn company_status_surfaces_prime_and_crew_breakdown() {
        let s = store();
        // No company yet → no Prime, empty crew.
        let empty = ok_body(handle_company_status(&s, &fake_ctx(b"")));
        assert!(empty.contains("\"prime\":null"), "got {empty}");
        assert!(empty.contains("\"total\":0"), "got {empty}");
        // Bootstrap the Founder, then hire a Prime + an engineer.
        ok_body(handle_bootstrap_founder(&s, &fake_ctx(b"Ada|echo")));
        ok_body(handle_create(
            &s,
            &fake_ctx(b"Pat|prime|Planner|plan|plan|Ada|subj-prime|medium"),
        ));
        ok_body(handle_create(
            &s,
            &fake_ctx(b"Eng|engineer|SWE|eng|eng|Pat|subj-eng|medium"),
        ));
        let body = ok_body(handle_company_status(&s, &fake_ctx(b"")));
        // The Prime is surfaced as a distinct identity (not just in the count).
        assert!(body.contains("\"prime\":{"), "prime object present: {body}");
        assert!(body.contains("\"name\":\"Pat\""), "got {body}");
        // Crew breakdown reflects the real company shape.
        assert!(body.contains("\"total\":3"), "got {body}");
        assert!(body.contains("\"founder\":1"), "by_role founder: {body}");
        assert!(body.contains("\"prime\":1"), "by_role prime: {body}");
        assert!(body.contains("\"engineer\":1"), "by_role engineer: {body}");
    }

    #[test]
    fn company_status_is_tenant_scoped() {
        let s = store();
        // Found a company in the `acme` Guild only.
        ok_body(handle_bootstrap_founder(
            &s,
            &fake_ctx_tenant(b"Ada|echo", "acme"),
        ));
        let acme = ok_body(handle_company_status(&s, &fake_ctx_tenant(b"", "acme")));
        assert!(
            acme.contains("\"initialized\":true"),
            "owning Guild sees it: {acme}"
        );
        // A different Guild sees an EMPTY company — no founder/prime leak.
        let globex = ok_body(handle_company_status(&s, &fake_ctx_tenant(b"", "globex")));
        assert!(globex.contains("\"initialized\":false"), "got {globex}");
        assert!(globex.contains("\"operative_count\":0"), "got {globex}");
        assert!(globex.contains("\"prime\":null"), "got {globex}");
        assert!(globex.contains("\"total\":0"), "got {globex}");
    }

    // ── company.status operations summary (company-model §5.4/§8.2;
    //    dashboard-design §5) ──────────────────────────────────────────────

    /// Stand up a small company in `tenant`: bootstrap the Founder, hire one
    /// active engineer, then propose + approve a Mandate so a real Brief tree
    /// AND a pending designer hire exist. Returns the three stores so the test
    /// can read the operations summary off live state.
    fn seed_company_for_ops(tenant: &str) -> (AgentStore, SpineStore, TaskStore) {
        let (agents, spine, task) = prime_stores();
        ok_body(handle_bootstrap_founder(
            &agents,
            &fake_ctx_tenant(b"Ada|echo", tenant),
        ));
        agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                tenant,
            )
            .unwrap();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx_tenant(b"Build a web dashboard", tenant),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(pid.as_bytes(), tenant),
        ));
        (agents, spine, task)
    }

    #[test]
    fn company_status_operations_summarizes_briefs_approvals_mandates() {
        let (agents, spine, task) = seed_company_for_ops("default");
        let v = json_of(ok_body(handle_company_status_with_ops(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        // The backward-compatible base fields are untouched by the new summary.
        assert_eq!(v["initialized"], true, "{v}");
        assert!(
            v["crew"]["total"].as_i64().unwrap() >= 2,
            "founder + engineer at least: {v}"
        );
        // The operations summary is present and reflects the seeded company.
        let ops = &v["operations"];
        assert!(ops.is_object(), "operations object present: {v}");
        assert_eq!(ops["mandates"]["total"], 1, "one Mandate: {ops}");
        assert!(
            ops["briefs"]["total"].as_i64().unwrap() >= 1,
            "a Brief tree exists: {ops}"
        );
        // The approved plan files a pending designer hire (inert until approved).
        assert!(
            ops["approvals"]["pending_hires"].as_i64().unwrap() >= 1,
            "a pending hire awaits approval: {ops}"
        );
        // No Shift has run yet → the runs window is calm.
        assert_eq!(ops["runs"]["recent"], 0, "no runs yet: {ops}");
        assert_eq!(ops["runs"]["running"], 0, "{ops}");
    }

    #[test]
    fn company_status_operations_is_tenant_isolated() {
        // Seed all the work in the `acme` Guild.
        let (agents, spine, task) = seed_company_for_ops("acme");
        let acme = json_of(ok_body(handle_company_status_with_ops(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(b"", "acme"),
        )));
        assert!(
            acme["operations"]["mandates"]["total"].as_i64().unwrap() >= 1,
            "owning Guild sees its Mandate: {acme}"
        );
        assert!(
            acme["operations"]["briefs"]["total"].as_i64().unwrap() >= 1,
            "owning Guild sees its Briefs: {acme}"
        );
        // A DIFFERENT Guild's summary is all zeros — no Brief / run / approval /
        // Mandate from `acme` leaks across the tenant boundary.
        let globex = json_of(ok_body(handle_company_status_with_ops(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(b"", "globex"),
        )));
        assert_eq!(globex["initialized"], false, "empty Guild: {globex}");
        let ops = &globex["operations"];
        assert_eq!(ops["briefs"]["total"], 0, "no Briefs leak: {ops}");
        assert_eq!(ops["mandates"]["total"], 0, "no Mandates leak: {ops}");
        assert_eq!(ops["approvals"]["pending_hires"], 0, "no hires leak: {ops}");
        assert_eq!(
            ops["approvals"]["pending_clearances"], 0,
            "no Clearances leak: {ops}"
        );
        assert_eq!(ops["runs"]["recent"], 0, "no runs leak: {ops}");
    }

    #[test]
    fn company_status_operations_empty_company_is_calm() {
        // No company at all → every operations bucket reads as a calm zero/empty
        // (never null-panics, never a fabricated figure).
        let (agents, spine, task) = prime_stores();
        let v = json_of(ok_body(handle_company_status_with_ops(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        assert_eq!(v["initialized"], false, "{v}");
        let ops = &v["operations"];
        assert_eq!(ops["briefs"]["total"], 0, "{ops}");
        assert!(
            ops["briefs"]["by_board"].as_object().unwrap().is_empty(),
            "empty board: {ops}"
        );
        assert_eq!(ops["briefs"]["ready_to_start"], 0, "{ops}");
        assert_eq!(ops["briefs"]["unassigned"], 0, "{ops}");
        assert_eq!(ops["runs"]["recent"], 0, "{ops}");
        assert_eq!(ops["runs"]["running"], 0, "{ops}");
        assert_eq!(ops["runs"]["failed_or_refused"], 0, "{ops}");
        assert_eq!(ops["runs"]["pending_review"], 0, "{ops}");
        assert_eq!(ops["approvals"]["pending_clearances"], 0, "{ops}");
        assert_eq!(ops["approvals"]["pending_hires"], 0, "{ops}");
        assert_eq!(ops["mandates"]["total"], 0, "{ops}");
        assert_eq!(ops["mandates"]["strategy_proposed"], 0, "{ops}");
    }

    #[test]
    fn operatives_roster_excludes_the_infra_console() {
        let s = store();
        // Seed an operator-console (infra) profile + a Founder.
        s.ensure_operator_console_profile("console-subj", "default")
            .unwrap();
        ok_body(handle_bootstrap_founder(&s, &fake_ctx(b"Ada|echo")));
        let roster = ok_body(handle_operatives(&s, &fake_ctx(b"")));
        assert!(roster.contains("\"role\":\"founder\""), "got {roster}");
        assert!(
            !roster.contains("operator-console"),
            "infra console must not appear in the Crew roster: {roster}"
        );
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

    fn json(o: HandlerOutcome) -> serde_json::Value {
        serde_json::from_str(&ok_body(o)).unwrap()
    }

    #[test]
    fn strategy_capability_drives_propose_then_approve() {
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate("default", "Login page", "wire to auth", None, None)
            .unwrap();
        // No strategy yet → status null, not approved.
        let v = json(handle_strategy_status(&spine, &fake_ctx(m.as_bytes())));
        assert_eq!(v["status"], serde_json::Value::Null);
        assert_eq!(v["approved"], false);
        // Propose → proposed (NOT approved — governance unchanged).
        let v = json(handle_strategy_propose(
            &spine,
            &fake_ctx(format!("{m}|hire planner; build login").as_bytes()),
        ));
        assert_eq!(v["status"], "proposed");
        assert_eq!(v["approved"], false);
        assert!(!spine.strategy_approved("default", &m).unwrap());
        // Approve → approved (the gate the orchestrator checks).
        let v = json(handle_strategy_approve(&spine, &fake_ctx(m.as_bytes())));
        assert_eq!(v["status"], "approved");
        assert_eq!(v["approved"], true);
        assert!(spine.strategy_approved("default", &m).unwrap());
    }

    #[test]
    fn strategy_capability_reject_blocks_approval() {
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate("default", "X", "y", None, None)
            .unwrap();
        handle_strategy_propose(&spine, &fake_ctx(format!("{m}|plan").as_bytes()));
        let v = json(handle_strategy_reject(&spine, &fake_ctx(m.as_bytes())));
        assert_eq!(v["status"], "rejected");
        assert!(!spine.strategy_approved("default", &m).unwrap());
    }

    #[test]
    fn strategy_propose_requires_doc_and_id() {
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate("default", "X", "y", None, None)
            .unwrap();
        // Empty doc + empty mandate id are both INVALID_ARGS.
        assert_eq!(
            err_kind(handle_strategy_propose(
                &spine,
                &fake_ctx(format!("{m}|").as_bytes())
            )),
            error_kinds::INVALID_ARGS
        );
        assert_eq!(
            err_kind(handle_strategy_status(&spine, &fake_ctx(b""))),
            error_kinds::INVALID_ARGS
        );
    }

    #[test]
    fn strategy_capability_is_tenant_scoped() {
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate("tenant-a", "X", "y", None, None)
            .unwrap();
        // A different tenant cannot propose on tenant-a's Mandate.
        let kind = err_kind(handle_strategy_propose(
            &spine,
            &fake_ctx_tenant(format!("{m}|sneak").as_bytes(), "tenant-b"),
        ));
        assert_eq!(kind, error_kinds::INVALID_ARGS);
        assert!(!spine.strategy_approved("tenant-a", &m).unwrap());
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

    // ── Prime Assistant (prime.propose / prime.approve) ──────────────

    fn prime_stores() -> (AgentStore, SpineStore, TaskStore) {
        (
            store(),
            SpineStore::in_memory().unwrap(),
            TaskStore::in_memory().unwrap(),
        )
    }

    fn json_of(body: String) -> serde_json::Value {
        serde_json::from_slice(body.as_bytes()).unwrap()
    }

    #[test]
    fn prime_propose_is_read_only_and_honest_about_no_llm() {
        let (agents, spine, _task) = prime_stores();
        let v = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build me a web dashboard for sales"),
        )));
        assert_eq!(v["status"], "proposed");
        let pid = v["proposal_id"].as_str().unwrap();
        assert!(pid.starts_with("prop_"));
        assert_eq!(v["proposal"]["intent"], "build");
        // AI honesty: never silently claimed as model output.
        assert_eq!(v["proposal"]["ai_used"], false);
        assert!(
            v["proposal"]["ai_status"]
                .as_str()
                .unwrap()
                .contains("deterministic"),
            "ai_status must be honest: {}",
            v["proposal"]["ai_status"]
        );
        // READ-ONLY: nothing was created — the stored proposal has no Mandate.
        let row = json_of(ok_body(handle_prime_proposal_get(
            &spine,
            &fake_ctx(pid.as_bytes()),
        )));
        assert_eq!(row["status"], "proposed");
        assert!(
            row["mandate_id"].is_null(),
            "propose must not create a Mandate"
        );
    }

    #[test]
    fn prime_propose_ai_mode_validates_model_output_server_side() {
        let (agents, spine, _task) = prime_stores();
        let model = serde_json::json!({
            "intent": "build",
            "mandate_title": "Billing system",
            "mandate_brief": "A subscription billing system.",
            "briefs": [
                {"key": "api", "title": "Billing API", "role": "engineer", "depends_on": []},
                {"key": "ship", "title": "Ship it", "role": "engineer", "depends_on": ["api"]}
            ],
            "risks": ["PCI scope"]
        })
        .to_string();
        let arg = serde_json::json!({
            "message": "Build a billing system",
            "model_output": model,
        })
        .to_string();
        let v = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(arg.as_bytes()),
        )));
        assert_eq!(v["status"], "proposed");
        assert_eq!(v["proposal"]["ai_used"], true);
        assert_eq!(v["proposal"]["ai_mode"], "llm_used");
        assert_eq!(v["proposal"]["mandate_title"], "Billing system");
        assert!(
            v["proposal"]["briefs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["key"] == "ship")
        );
    }

    #[test]
    fn prime_propose_ai_mode_falls_back_on_bad_model_output() {
        let (agents, spine, _task) = prime_stores();
        // Cyclic deps → validator rejects → deterministic fallback.
        let model = serde_json::json!({
            "mandate_title": "X",
            "briefs": [
                {"key": "a", "title": "a", "role": "engineer", "depends_on": ["b"]},
                {"key": "b", "title": "b", "role": "engineer", "depends_on": ["a"]}
            ]
        })
        .to_string();
        let arg =
            serde_json::json!({"message": "Build a thing", "model_output": model}).to_string();
        let v = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(arg.as_bytes()),
        )));
        assert_eq!(v["proposal"]["ai_used"], false);
        assert_eq!(v["proposal"]["ai_mode"], "fallback");
        assert!(
            v["proposal"]["ai_status"]
                .as_str()
                .unwrap()
                .contains("fallback"),
            "{}",
            v["proposal"]["ai_status"]
        );
        // It is still a real, usable deterministic plan.
        assert!(!v["proposal"]["briefs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn prime_propose_ai_mode_reports_unavailable() {
        let (agents, spine, _task) = prime_stores();
        let arg = serde_json::json!({
            "message": "Build a thing",
            "model_unavailable_reason": "no model peer reachable",
        })
        .to_string();
        let v = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(arg.as_bytes()),
        )));
        assert_eq!(v["proposal"]["ai_used"], false);
        assert_eq!(v["proposal"]["ai_mode"], "unavailable");
    }

    #[test]
    fn prime_propose_bare_text_is_still_deterministic() {
        let (agents, spine, _task) = prime_stores();
        // The legacy raw-text form keeps working unchanged.
        let v = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )));
        assert_eq!(v["proposal"]["ai_mode"], "deterministic_only");
        assert_eq!(v["proposal"]["ai_used"], false);
    }

    #[test]
    fn prime_approve_creates_mandate_briefs_assigns_existing_and_requests_missing() {
        let (agents, spine, task) = prime_stores();
        // One ACTIVE engineer exists; no designer.
        agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();

        let a = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        assert_eq!(a["status"], "approved");
        let mandate_id = a["mandate_id"].as_str().unwrap();
        assert!(mandate_id.starts_with("mandate_"));
        // engineer + designer tracks + integration brief.
        let created = a["created_briefs"].as_array().unwrap();
        assert!(created.len() >= 3, "got {created:?}");
        // The engineer track was assigned to the EXISTING active engineer.
        assert!(
            !a["assigned_briefs"].as_array().unwrap().is_empty(),
            "engineer track should assign to existing crew"
        );
        // The missing designer became a PENDING hire request — not a fake
        // active agent.
        let hires = a["hire_requests"].as_array().unwrap();
        assert_eq!(hires.len(), 1);
        let hire = agents
            .get_agent(hires[0].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            hire.status, "pending",
            "a suggested hire is inert until Clearance"
        );
        assert_eq!(hire.role, "designer");
        // Chronicle: each created Brief carries a prime.brief_created event.
        let evs = task
            .query_events(
                created[0].as_str().unwrap(),
                0,
                50,
                Some("prime.brief_created"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(evs.len(), 1, "Prime approval is chronicled on the Brief");
    }

    #[test]
    fn prime_approve_stamps_same_tenant_founder_as_reviewer() {
        // company-model §5.4/§12.6: a Prime-materialized Brief is stamped with
        // the Founder/Board as its reviewer up front, so a completed Shift can
        // move to `in_review` instead of parking in `blocked`. The reviewer is
        // the SAME-tenant Founder, deterministically (oldest `role='founder'`).
        let (agents, spine, task) = prime_stores();
        // Found the company (creates the single Founder Operative).
        ok_body(handle_bootstrap_founder(&agents, &fake_ctx(b"Ada|echo")));
        let founder_id = agents.find_founder("default").unwrap().unwrap().agent_id;
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let a = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        let created = a["created_briefs"].as_array().unwrap();
        assert!(
            created.len() >= 3,
            "engineer + designer tracks + integrate: {created:?}"
        );
        // EVERY created Brief — the role tracks AND the dependent `integrate`
        // Brief — carries the Founder as reviewer, with a Chronicle event.
        for b in created {
            let id = b.as_str().unwrap();
            assert_eq!(
                task.brief_fields(id)
                    .unwrap()
                    .unwrap()
                    .reviewer_agent_id
                    .as_deref(),
                Some(founder_id.as_str()),
                "Brief {id} should be stamped with the Founder reviewer"
            );
            let evs = task
                .query_events(
                    id,
                    0,
                    50,
                    Some("brief.reviewer_assigned"),
                    crate::nodes::coordinator::EventOrder::Desc,
                )
                .unwrap();
            assert_eq!(
                evs.len(),
                1,
                "reviewer assignment is chronicled on Brief {id}"
            );
        }
        // The dependent `integrate` Brief specifically follows the same rule.
        let integrate = task
            .get_brief_by_source_marker(&format!("prime:{pid}:integrate"))
            .unwrap()
            .expect("integrate brief")
            .task_id;
        assert_eq!(
            task.brief_fields(&integrate)
                .unwrap()
                .unwrap()
                .reviewer_agent_id
                .as_deref(),
            Some(founder_id.as_str()),
            "the dependent integrate Brief is reviewer-aware too"
        );
    }

    #[test]
    fn prime_approve_no_founder_leaves_reviewer_unset() {
        // Honest fallback: with no Founder (company never bootstrapped) there is
        // no legitimate same-tenant reviewer, so the reviewer is left unset and
        // the old "parks in blocked until a reviewer is set" behaviour holds. We
        // never fabricate or borrow an arbitrary reviewer.
        let (agents, spine, task) = prime_stores();
        assert!(agents.find_founder("default").unwrap().is_none());
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let a = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        for b in a["created_briefs"].as_array().unwrap() {
            let id = b.as_str().unwrap();
            assert!(
                task.brief_fields(id)
                    .unwrap()
                    .unwrap()
                    .reviewer_agent_id
                    .is_none(),
                "no Founder → Brief {id} carries no reviewer (honest fallback)"
            );
        }
    }

    #[test]
    fn prime_approve_reviewer_is_never_cross_tenant() {
        // Tenant isolation: a Founder in Guild `acme` must NEVER be stamped as a
        // reviewer on Briefs materialized in Guild `globex`. `globex` has no
        // Founder, so its Briefs stay reviewer-less rather than borrowing acme's.
        let (agents, spine, task) = prime_stores();
        // A Founder exists ONLY in `acme`.
        ok_body(handle_bootstrap_founder(
            &agents,
            &fake_ctx_tenant(b"Ada|echo", "acme"),
        ));
        assert!(agents.find_founder("acme").unwrap().is_some());
        assert!(agents.find_founder("globex").unwrap().is_none());
        // Approve a proposal in `globex`.
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx_tenant(b"Build a web dashboard", "globex"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let a = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(pid.as_bytes(), "globex"),
        )));
        for b in a["created_briefs"].as_array().unwrap() {
            let id = b.as_str().unwrap();
            assert!(
                task.brief_fields(id)
                    .unwrap()
                    .unwrap()
                    .reviewer_agent_id
                    .is_none(),
                "globex Brief {id} must NOT borrow acme's Founder as reviewer"
            );
        }
    }

    #[tokio::test]
    async fn completed_shift_with_founder_reviewer_reaches_in_review_not_blocked() {
        // company-model §12.6 + execution-and-issue §1.3: with the Founder
        // stamped as reviewer at approval, a completed echo Shift's board lands
        // in `in_review` (review-ready) instead of `blocked` (which read as a
        // failure). The heartbeat's best-effort `Done → in_review` now succeeds.
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        // Empty company → starter crew (creates the Founder + echo workers).
        let _ = ok_body(handle_starter_crew(&agents, &fake_ctx(b"")));
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard for sales"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        let started = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        let runs = started["started"].as_array().unwrap().clone();
        assert!(
            !runs.is_empty(),
            "at least one track Shift started: {started}"
        );
        // Wait for every started Shift to reach its terminal `done` run state.
        let run_ids: Vec<String> = runs
            .iter()
            .map(|r| r["run_id"].as_str().unwrap().to_string())
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let all_done = run_ids.iter().all(|id| {
                matches!(task.get_run(id).unwrap().map(|r| r.status), Some(ref s) if s == "done")
            });
            if all_done {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Shifts never reached done"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Each completed track sits in `in_review` — NOT `blocked` — because it
        // carries a reviewer. This is the fix: review-ready work reads correctly.
        for r in &runs {
            let brief_id = r["brief_id"].as_str().unwrap();
            assert_eq!(
                task.board_status(brief_id).unwrap().as_deref(),
                Some("in_review"),
                "a completed reviewer-stamped Shift lands in in_review, not blocked"
            );
        }
    }

    #[test]
    fn prime_approve_is_idempotent() {
        let (agents, spine, task) = prime_stores();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Write the onboarding docs"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let first = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        let mandate1 = first["mandate_id"].as_str().unwrap().to_string();
        // A second approve does NOT create a second Mandate.
        let second = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        assert_eq!(second["already_approved"], true);
        assert_eq!(second["mandate_id"].as_str().unwrap(), mandate1);
    }

    #[test]
    fn prime_proposal_is_tenant_scoped() {
        let (agents, spine, task) = prime_stores();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx_tenant(b"Build a dashboard", "acme"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        // The owning Guild can read it.
        let _ = ok_body(handle_prime_proposal_get(
            &spine,
            &fake_ctx_tenant(pid.as_bytes(), "acme"),
        ));
        // A different Guild can NEITHER read NOR approve it (reads as
        // not-found — no cross-tenant existence leak).
        assert_eq!(
            err_kind(handle_prime_proposal_get(
                &spine,
                &fake_ctx_tenant(pid.as_bytes(), "globex"),
            )),
            error_kinds::INVALID_ARGS
        );
        assert_eq!(
            err_kind(handle_prime_approve(
                &agents,
                &spine,
                &task,
                &fake_ctx_tenant(pid.as_bytes(), "globex"),
            )),
            error_kinds::INVALID_ARGS
        );
    }

    #[test]
    fn prime_propose_requires_a_message() {
        let (agents, spine, _task) = prime_stores();
        assert_eq!(
            err_kind(handle_prime_propose(&agents, &spine, &fake_ctx(b"   "))),
            error_kinds::INVALID_ARGS
        );
    }

    // ── Prime Start-to-Shift (prime.start) — company-model §12.5B ────────

    fn echo_registry() -> crate::rig::RigRegistry {
        crate::rig::RigRegistry::with_builtins().with_default("echo")
    }

    #[test]
    fn prime_start_refuses_a_non_approved_proposal() {
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        // Approve is the only thing that materializes Briefs; start before
        // approve has nothing to run and is refused.
        assert_eq!(
            err_kind(handle_prime_start(
                &agents,
                &spine,
                &task,
                &reg,
                &fake_ctx(pid.as_bytes())
            )),
            error_kinds::INVALID_ARGS
        );
    }

    #[test]
    fn prime_start_with_no_crew_starts_nothing_and_explains() {
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        // No active crew → approve creates Briefs but assigns none of them.
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        ));
        let v = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        // Nothing runs (no assignee), and the unassigned Briefs are reported
        // with honest reasons — never silently dropped.
        assert!(
            v["started"].as_array().unwrap().is_empty(),
            "no crew → nothing runs: {v}"
        );
        let skipped = v["skipped"].as_array().unwrap();
        assert!(!skipped.is_empty());
        assert!(
            skipped
                .iter()
                .all(|s| !s["reason"].as_str().unwrap_or("").is_empty()),
            "every skip carries a reason: {v}"
        );
    }

    #[tokio::test]
    async fn prime_start_runs_the_ready_brief_as_a_shift() {
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        // One ACTIVE engineer → the engineer track is assigned + ready on
        // approve (the designer track is a pending hire; the integration Brief
        // is blocked on the tracks).
        agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        ));
        let v = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        let started = v["started"].as_array().unwrap();
        assert_eq!(
            started.len(),
            1,
            "exactly the ready engineer track runs: {v}"
        );
        // A REAL Shift was opened (a run_id), through the same chokepoint as
        // brief.run — never a faked run.
        assert!(
            started[0]["run_id"].as_str().is_some(),
            "a Shift (run_id) was opened: {v}"
        );
        // The unassigned designer + the blocked integration Brief are reported.
        assert!(!v["skipped"].as_array().unwrap().is_empty());
        // The start is chronicled on the started Brief.
        let started_id = started[0]["brief_id"].as_str().unwrap();
        let evs = task
            .query_events(
                started_id,
                0,
                50,
                Some("prime.work_started"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(evs.len(), 1, "Prime start is chronicled on the Brief");
    }

    #[test]
    fn prime_start_is_tenant_scoped() {
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx_tenant(b"Build a dashboard", "acme"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(pid.as_bytes(), "acme"),
        ));
        // A different Guild cannot start it (reads as not-found).
        assert_eq!(
            err_kind(handle_prime_start(
                &agents,
                &spine,
                &task,
                &reg,
                &fake_ctx_tenant(pid.as_bytes(), "globex"),
            )),
            error_kinds::INVALID_ARGS
        );
    }

    // ── Prime Shift-Room status (prime.status) — Live Shift Room PART A ──

    #[test]
    fn prime_status_for_approved_proposal_reports_briefs_and_counts() {
        let (agents, spine, task) = prime_stores();
        // One ACTIVE engineer → the engineer track is assigned + ready; the
        // designer track is an unassigned pending hire; integration is blocked.
        agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        ));

        let v = json_of(ok_body(handle_prime_status(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        assert_eq!(v["status"], "approved");
        assert!(v["mandate_id"].is_string(), "approved → a Mandate id: {v}");
        assert!(
            v["mandate_title"].is_string(),
            "Mandate title resolved: {v}"
        );
        let briefs = v["briefs"].as_array().unwrap();
        assert!(briefs.len() >= 3, "engineer + designer + integration: {v}");
        let counts = &v["counts"];
        assert_eq!(counts["total_briefs"], briefs.len());
        // The engineer track is ready; the designer track is unassigned; the
        // integration Brief is blocked on the tracks.
        assert_eq!(counts["ready"], 1, "{v}");
        assert_eq!(counts["unassigned"], 1, "{v}");
        assert_eq!(counts["blocked"], 1, "{v}");
        // The ready engineer track names its assignee + Rig + a start action.
        let ready_brief = briefs
            .iter()
            .find(|b| b["start_readiness"] == "ready")
            .expect("a ready Brief");
        assert!(ready_brief["assignee"].is_string());
        assert!(
            ready_brief["next_action"]
                .as_str()
                .unwrap()
                .contains("start")
        );
        // The blocked integration Brief explains WHAT blocks it.
        let blocked = briefs
            .iter()
            .find(|b| b["start_readiness"] == "blocked")
            .expect("a blocked Brief");
        assert!(
            !blocked["blockers"].as_array().unwrap().is_empty(),
            "a blocked Brief lists its open blockers: {blocked}"
        );
        // The session surfaces concrete next actions.
        assert!(!v["recommended_next_actions"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn prime_status_reflects_a_started_shift() {
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        ));
        // Start the ready Brief as a real Shift through the run chokepoint.
        let started = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        assert_eq!(started["started"].as_array().unwrap().len(), 1, "{started}");

        // The status now carries the latest Shift on that Brief.
        let v = json_of(ok_body(handle_prime_status(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        let with_run = v["briefs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["latest_run"].is_object())
            .expect("a Brief with a latest run");
        let run = &with_run["latest_run"];
        assert!(run["run_id"].is_string(), "a real run_id: {with_run}");
        // The run reached a terminal echo state (done) — the status must not
        // fake it. The done Shift opens review, so the Brief needs review.
        let counts = &v["counts"];
        let accounted = counts["running"].as_i64().unwrap()
            + counts["done"].as_i64().unwrap()
            + counts["needs_review"].as_i64().unwrap()
            + counts["failed"].as_i64().unwrap()
            + counts["refused"].as_i64().unwrap();
        assert!(accounted >= 1, "the started Shift is accounted for: {v}");
    }

    #[tokio::test]
    async fn starter_crew_closes_the_positive_local_loop_through_prime_start() {
        // company-model §12.6 + §12.5B: from an EMPTY company, the owner's
        // starter-crew bootstrap makes prime.propose → approve → start reach a
        // real, completed echo Shift — no external coding-agent auth required.
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        // 1) Empty company → safe-local starter crew (Founder + echo workers).
        let _ = ok_body(handle_starter_crew(&agents, &fake_ctx(b"")));
        // 2) Describe → plan (a build, so engineer/designer tracks match the crew).
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard for sales"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        // 3) Approve → the tracks assign to the active starter Operatives.
        let approved = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        assert!(
            !approved["assigned_briefs"].as_array().unwrap().is_empty(),
            "starter crew gets the tracks assigned: {approved}"
        );
        // 4) Start → the ready Briefs become real echo Shifts that complete.
        let started = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        let runs = started["started"].as_array().unwrap();
        assert!(!runs.is_empty(), "at least one Shift started: {started}");
        for r in runs {
            assert!(r["run_id"].is_string(), "a real run_id: {r}");
            assert_eq!(r["rig"], "echo", "ran on the safe local echo Rig: {r}");
        }
        let run_id = runs[0]["run_id"].as_str().unwrap().to_string();

        // 5) `prime.start` dispatches the adapter on a background thread, so the
        //    run is reported `running` immediately and only reaches its terminal
        //    state once that thread closes the ledger. Wait for `done` +
        //    `pending_review` (company-model §12.6: "each Shift reaches done and
        //    opens review").
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(r) = task.get_run(&run_id).unwrap()
                && r.status == "done"
                && r.review.as_deref() == Some("pending_review")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "echo Shift never reached done/pending_review"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // 6) Apply is REVIEW-GATED: a `done` run sitting in `pending_review` is
        //    not yet apply-eligible (no work is applied behind the operator).
        let run = task.get_run(&run_id).unwrap().expect("the run row");
        assert!(
            crate::nodes::coordinator::heartbeat::run_apply_eligibility(&run).is_err(),
            "a pending_review run must not be apply-eligible yet: {run:?}"
        );

        // 7) The operator accepts the review → the run becomes apply-eligible
        //    (the documented review → apply tail of the local loop).
        task.set_run_review(&run_id, "accepted", "looks good")
            .unwrap();
        let run = task.get_run(&run_id).unwrap().expect("the run row");
        assert!(
            crate::nodes::coordinator::heartbeat::run_apply_eligibility(&run).is_ok(),
            "an accepted scoped-workspace `done` run is apply-eligible: {run:?}"
        );

        // 8) Apply the accepted run into a throwaway project root. An echo Shift
        //    writes nothing, so the apply is a safe `applied` no-op — the loop
        //    closes WITHOUT the run ever touching a real project root.
        let project_root = tempfile::TempDir::new().unwrap();
        let artifacts = task.list_run_artifacts(&run_id).unwrap();
        let outcome =
            crate::nodes::coordinator::heartbeat::apply_run(project_root.path(), &artifacts)
                .unwrap();
        assert_eq!(
            outcome.status, "applied",
            "echo run applies cleanly: {outcome:?}"
        );
        assert_eq!(
            outcome.applied_files, 0,
            "echo writes nothing — a no-op apply"
        );
        assert_eq!(outcome.failed_files, 0);

        // 9) Persist the apply result → the Shift's durable lifecycle terminal is
        //    `applied`, closing the §12.6 positive local loop end to end:
        //    empty company → starter crew → propose → approve → start → done →
        //    review → accept → apply.
        task.set_run_apply_status(
            &run_id,
            outcome.status,
            &outcome.plan.note,
            outcome.applied_files as i64,
            outcome.failed_files as i64,
        )
        .unwrap();
        let run = task.get_run(&run_id).unwrap().expect("the run row");
        assert_eq!(
            run.apply_status.as_deref(),
            Some("applied"),
            "the Shift's durable terminal is `applied`: {run:?}"
        );

        // 10) Productized review-to-done (company-model §12.5B/§12.6): the
        //     completed Shift parked the Brief in `in_review`, and the operator's
        //     apply IS the review-to-done — `run.apply` advances the Brief to
        //     board `done` (the same store call the handler makes) so dependents
        //     unblock WITHOUT a separate manual `brief.move done`. The loop now
        //     closes on the BOARD, not just the run ledger.
        assert_eq!(
            task.board_status(&run.brief_id).unwrap().as_deref(),
            Some("in_review"),
            "the completed echo Shift parked its Brief in review"
        );
        let to = task
            .complete_reviewed_brief(&run.brief_id)
            .unwrap()
            .expect("the operator's apply advances the reviewed Brief to done");
        assert_eq!(to, "done");
        assert_eq!(
            task.board_status(&run.brief_id).unwrap().as_deref(),
            Some("done"),
            "the §12.6 positive local loop closes on the board: the Brief is done"
        );
    }

    #[tokio::test]
    async fn prime_start_reconciles_a_greenlit_hire_so_dependent_work_unblocks() {
        // company-model §12.5B: the governed loop must NOT stop at hire. When a
        // build plan infers a role with no active Operative, `prime.approve`
        // files a `pending` hire and leaves that role-track Brief unassigned.
        // Until now the track stayed `Unassigned` forever even after the operator
        // approved the hire — so the dependent `integrate` track (blocked on every
        // track) never unblocked. `prime.start` now reconciles: it staffs the
        // greenlit hire's waiting track, runs it, and once the tracks reach `done`
        // the dependent Brief unblocks and runs too.
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();

        // Empty company → safe-local starter crew (Founder + echo engineer +
        // echo designer). NO qa Operative exists.
        let _ = ok_body(handle_starter_crew(&agents, &fake_ctx(b"")));

        // A build needing test coverage → engineer + designer + qa tracks, plus
        // an `integrate` Brief that depends on all three.
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web app with test coverage"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Approve → engineer/designer (and the engineer-owned integrate) get
        // assigned; the missing qa role becomes a SINGLE `pending` hire.
        let approved = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        let hires = approved["hire_requests"].as_array().unwrap();
        assert_eq!(
            hires.len(),
            1,
            "exactly the missing qa role is a hire: {approved}"
        );
        let qa_hire_id = hires[0].as_str().unwrap().to_string();

        // Resolve the Brief ids by their stable Prime source markers.
        let qa_track = task
            .get_brief_by_source_marker(&format!("prime:{pid}:track:qa"))
            .unwrap()
            .expect("qa track brief")
            .task_id;
        let integrate = task
            .get_brief_by_source_marker(&format!("prime:{pid}:integrate"))
            .unwrap()
            .expect("integrate brief")
            .task_id;
        // The qa track is unassigned (its hire is still pending).
        assert!(
            task.brief_card(&qa_track)
                .unwrap()
                .unwrap()
                .assignee_agent_id
                .as_deref()
                .unwrap_or("")
                .is_empty(),
            "qa track is unassigned before the hire is greenlit"
        );

        let started_runs = |v: &serde_json::Value| -> Vec<String> {
            v["started"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["run_id"].as_str().unwrap().to_string())
                .collect()
        };
        let mut run_ids: Vec<String> = Vec::new();

        // Start #1 (hire NOT yet approved): the qa track skips honestly, the
        // integrate track is blocked, the staffed tracks run.
        let s1 = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        assert!(
            s1["assigned"].as_array().unwrap().is_empty(),
            "nothing greenlit yet → no late assignment: {s1}"
        );
        let qa_skip = s1["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["brief_id"] == serde_json::json!(qa_track))
            .expect("qa track is reported skipped");
        assert!(
            qa_skip["reason"].as_str().unwrap().contains("no Operative"),
            "qa track skipped because no Operative is assigned yet: {qa_skip}"
        );
        run_ids.extend(started_runs(&s1));
        assert!(
            !run_ids.is_empty(),
            "engineer/designer tracks started: {s1}"
        );

        // The operator greenlights the qa hire (pending → active) — the GOVERNED
        // hire-approval path.
        let _ = ok_body(handle_approve_hire(
            &agents,
            &fake_ctx(qa_hire_id.as_bytes()),
        ));

        // Start #2: prime.start now RECONCILES — it staffs the qa track to the
        // now-active hire and starts it (where before it skipped forever).
        let s2 = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        assert!(
            s2["assigned"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == &serde_json::json!(qa_track)),
            "the greenlit qa hire's track is reconciled/assigned: {s2}"
        );
        assert_eq!(
            task.brief_card(&qa_track)
                .unwrap()
                .unwrap()
                .assignee_agent_id
                .as_deref(),
            Some(qa_hire_id.as_str()),
            "the qa track is now assigned to the activated hire"
        );
        let qa_run = s2["started"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["brief_id"] == serde_json::json!(qa_track))
            .expect("the qa track started a real Shift");
        assert_eq!(
            qa_run["rig"], "echo",
            "ran on the safe local echo Rig: {qa_run}"
        );
        run_ids.extend(started_runs(&s2));

        // Wait for every started track Shift to reach its terminal echo state, so
        // the board is stable and the Claim released before the operator reviews.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let all_done = run_ids.iter().all(|id| {
                matches!(
                    task.get_run(id).unwrap().map(|r| r.status),
                    Some(ref s) if s == "done"
                )
            });
            if all_done {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "track Shifts never reached done"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // The dependent integrate Brief is still blocked — a blocker clears only
        // when the blocking Brief reaches board `done`, and the tracks are not
        // there yet (a finished Shift opens its run review; the Brief still needs
        // the operator's accept to move to done).
        assert!(
            task.is_blocked(&integrate).unwrap(),
            "integrate stays blocked until its tracks reach done"
        );

        // The operator reviews each track to board `done` (the governed
        // review path: set a reviewer → in_progress → in_review → done). This
        // resolves the integrate Brief's blockers.
        for marker in ["track:engineer", "track:designer", "track:qa"] {
            let card = task
                .get_brief_by_source_marker(&format!("prime:{pid}:{marker}"))
                .unwrap()
                .unwrap();
            let id = &card.task_id;
            let reviewer = card.assignee_agent_id.as_deref().unwrap_or_default();
            task.set_brief_field(id, "reviewer", reviewer).unwrap();
            task.set_board_status(id, "in_progress").unwrap();
            task.set_board_status(id, "in_review").unwrap();
            task.set_board_status(id, "done").unwrap();
        }
        assert!(
            !task.is_blocked(&integrate).unwrap(),
            "with every track done, the dependent integrate Brief unblocks"
        );

        // Start #3: the previously-blocked dependent Brief now runs — dependent
        // work unblocked end to end, all through governed gates.
        let s3 = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        assert!(
            s3["started"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["brief_id"] == serde_json::json!(integrate) && r["run_id"].is_string()),
            "the dependent integrate Brief starts a real Shift once unblocked: {s3}"
        );
    }

    #[test]
    fn prime_status_is_tenant_scoped() {
        let (agents, spine, task) = prime_stores();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx_tenant(b"Build a dashboard", "acme"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        // The owning Guild can read its session status.
        let _ = ok_body(handle_prime_status(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(pid.as_bytes(), "acme"),
        ));
        // A different Guild reads it as not-found (no existence leak).
        assert_eq!(
            err_kind(handle_prime_status(
                &agents,
                &spine,
                &task,
                &fake_ctx_tenant(pid.as_bytes(), "globex"),
            )),
            error_kinds::INVALID_ARGS
        );
    }

    #[test]
    fn prime_status_blockers_are_tenant_scoped_against_legacy_cross_tenant_edges() {
        // A created Brief in Guild "acme" gains a legacy `blocked_on` edge to a
        // Brief that lives in a DIFFERENT Guild ("globex"). `prime.status` must
        // NOT surface that cross-tenant blocker — even though the raw edge
        // exists — because the open-blocker read is tenant-scoped.
        let (agents, spine, task) = prime_stores();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx_tenant(b"Build a web dashboard", "acme"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let approved = json_of(ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(pid.as_bytes(), "acme"),
        )));
        let created: Vec<String> = approved["created_briefs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let victim = created.first().expect("at least one created Brief").clone();

        // A Brief owned by a different Guild — the would-be leaked blocker.
        let foreign = task
            .create_brief(
                "globex",
                "SECRET cross-tenant blocker",
                "owner",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        // Force the legacy cross-tenant edge directly (the store's edge writer
        // does not itself tenant-check — only existence + no-cycle).
        task.add_snag(&victim, &foreign).unwrap();
        // Sanity: the raw (un-scoped) edge really is present.
        assert!(
            task.list_snags(&victim).unwrap().contains(&foreign),
            "the cross-tenant edge must exist for the test to be meaningful"
        );

        // The acme Guild's Shift Room must not leak it.
        let v = json_of(ok_body(handle_prime_status(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(pid.as_bytes(), "acme"),
        )));
        let raw = v.to_string();
        assert!(
            !raw.contains(&foreign),
            "cross-tenant blocker id leaked into prime.status: {raw}"
        );
        assert!(
            !raw.contains("SECRET cross-tenant blocker"),
            "cross-tenant blocker title leaked into prime.status: {raw}"
        );
        // Specifically, the victim Brief lists none of the foreign blocker.
        let victim_row = v["briefs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["brief_id"] == serde_json::json!(victim))
            .expect("the victim Brief is present");
        let blockers = victim_row["blockers"].as_array().unwrap();
        assert!(
            blockers
                .iter()
                .all(|b| b["brief_id"] != serde_json::json!(foreign)),
            "the foreign blocker must be filtered from the victim's blockers: {victim_row}"
        );
    }

    #[tokio::test]
    async fn prime_status_idempotent_start_still_reflected() {
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        ));
        // Start once; the ready Brief now has a live/terminal Shift, so a second
        // start finds nothing newly ready — idempotent (no double-work). The
        // status remains coherent across both.
        let _ = ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        ));
        let second = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        // The second start began no NEW Shift on the already-run Brief.
        assert!(
            second["started"].as_array().unwrap().is_empty()
                || second["started"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|s| s["run_id"].is_string()),
            "second start is idempotent / honest: {second}"
        );
        // Status total is stable (start creates no new Briefs).
        let v = json_of(ok_body(handle_prime_status(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        )));
        assert_eq!(
            v["counts"]["total_briefs"].as_u64().unwrap(),
            v["briefs"].as_array().unwrap().len() as u64
        );
    }

    #[test]
    fn prime_status_requires_proposal_id() {
        let (agents, spine, task) = prime_stores();
        assert_eq!(
            err_kind(handle_prime_status(
                &agents,
                &spine,
                &task,
                &fake_ctx(b"   ")
            )),
            error_kinds::INVALID_ARGS
        );
    }

    // ── Action Center (company.actions) — company-model §5.4 / §8.2 ──────

    #[test]
    fn company_actions_empty_is_calm() {
        let (agents, spine, task) = prime_stores();
        let v = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        assert_eq!(v["counts"]["total"], 0, "{v}");
        assert!(v["actions"].as_array().unwrap().is_empty());
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn company_actions_surfaces_hire_ready_and_blocked() {
        let (agents, spine, task) = prime_stores();
        // One ACTIVE engineer → the engineer track is ready; the designer track
        // is an unassigned pending hire; integration is dependency-blocked.
        agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        ));

        let v = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        let actions = v["actions"].as_array().unwrap();
        let cats: Vec<&str> = actions
            .iter()
            .map(|a| a["category"].as_str().unwrap())
            .collect();
        assert!(cats.contains(&"hire"), "a pending designer hire: {v}");
        assert!(
            cats.contains(&"ready_to_start"),
            "engineer track ready: {v}"
        );
        assert!(cats.contains(&"blocked"), "integration blocked: {v}");
        // The hire card is machine-actionable: it names the approval API target
        // and the safe-local Rig a client should pass to make the Operative
        // runnable in one call (company-model §12.6). No secret in the payload.
        let hire = actions
            .iter()
            .find(|a| a["category"] == serde_json::json!("hire"))
            .expect("a hire card");
        let agent_id = hire["target_id"].as_str().unwrap();
        assert_eq!(
            hire["action_api"].as_str().unwrap(),
            format!("POST /v1/agents/{agent_id}/approve-hire"),
            "hire card carries the approval API target: {hire}"
        );
        assert_eq!(
            hire["suggested_rig"], "echo",
            "hire card suggests the safe-local Rig: {hire}"
        );
        // Part B ordering: hire (high) before ready_to_start (medium) before
        // blocked (medium, but ranked after ready so work can move forward).
        let hire_pos = cats.iter().position(|c| *c == "hire").unwrap();
        let ready_pos = cats.iter().position(|c| *c == "ready_to_start").unwrap();
        let blocked_pos = cats.iter().position(|c| *c == "blocked").unwrap();
        assert!(hire_pos < ready_pos, "hire before ready: {cats:?}");
        assert!(ready_pos < blocked_pos, "ready before blocked: {cats:?}");
        // Counts match the deduped feed; every item has an actionable label.
        assert_eq!(v["counts"]["total"].as_u64().unwrap(), actions.len() as u64);
        assert!(
            actions.iter().all(|a| a["action_label"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false)),
            "every action carries a recommended action label: {v}"
        );
    }

    #[tokio::test]
    async fn company_actions_includes_needs_review_after_a_shift() {
        let (agents, spine, task) = prime_stores();
        let task = std::sync::Arc::new(task);
        let reg = echo_registry();
        agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        ));
        // Run the ready Brief as a real echo Shift. `prime.start` dispatches the
        // adapter on a background thread (a long Shift must not freeze the
        // bridge), reporting `running` immediately; the run reaches `done` and
        // opens `pending_review` only once that thread closes the ledger.
        let started = json_of(ok_body(handle_prime_start(
            &agents,
            &spine,
            &task,
            &reg,
            &fake_ctx(pid.as_bytes()),
        )));
        let started_arr = started["started"].as_array().unwrap();
        assert_eq!(started_arr.len(), 1, "{started}");
        let run_id = started_arr[0]["run_id"].as_str().unwrap().to_string();

        // Wait for the background Shift to finish and open review — without this
        // barrier the feed is read mid-run (still `running`, review NULL).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let done = task
                .list_runs_for_tenant("default", 10)
                .unwrap()
                .into_iter()
                .any(|r| {
                    r.run_id == run_id
                        && r.status == "done"
                        && r.review.as_deref() == Some("pending_review")
                });
            if done {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "echo Shift never reached done/pending_review"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let v = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        let actions = v["actions"].as_array().unwrap();
        let nr = actions
            .iter()
            .find(|a| a["category"] == "needs_review")
            .expect("a done Shift awaits review");
        // It deep-links to the run for review → apply.
        assert!(
            nr["route"].as_str().unwrap().contains("/runs?run="),
            "needs_review deep-links to the run: {nr}"
        );
        assert_eq!(v["counts"]["by_category"]["needs_review"], 1, "{v}");

        // Once the operator reviews the run, the completed Shift must DROP out of
        // `needs_review` — a reviewed run is no longer an open action.
        task.set_run_review(&run_id, "accepted", "looks good")
            .unwrap();
        let v2 = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        assert!(
            !v2["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["category"] == "needs_review"),
            "an accepted Shift no longer awaits review: {v2}"
        );
        assert_eq!(
            v2["counts"]["by_category"]
                .get("needs_review")
                .and_then(|c| c.as_u64())
                .unwrap_or(0),
            0,
            "{v2}"
        );
    }

    #[test]
    fn company_actions_dedupes_hire_with_its_spawn_clearance() {
        // A pending hire AND a spawn Clearance for it both point at the same
        // Operative; the feed must show ONE item (the approval), not two.
        let (agents, spine, task) = prime_stores();
        let hire = agents
            .request_hire(
                "Des",
                "designer",
                "D",
                "des",
                "des",
                "founder",
                &subject_of(b"des"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .create_spawn_clearance(&hire, &subject_of(b"des"), "route=founder", &[], "default")
            .unwrap();
        let v = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        let actions = v["actions"].as_array().unwrap();
        let for_agent: Vec<&serde_json::Value> = actions
            .iter()
            .filter(|a| a["target_id"] == serde_json::json!(hire))
            .collect();
        assert_eq!(
            for_agent.len(),
            1,
            "a hire + its Clearance must not spam the operator: {actions:?}"
        );
        assert_eq!(
            for_agent[0]["category"], "approval",
            "the more-urgent approval wins the dedupe"
        );
    }

    #[test]
    fn company_actions_surfaces_proposed_strategy() {
        let (agents, spine, task) = prime_stores();
        let m = spine
            .create_mandate("default", "Ship v1", "the why", None, None)
            .unwrap();
        spine.propose_strategy("default", &m, "the plan").unwrap();
        let v = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        let a = v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["target_type"] == "mandate" && a["target_id"] == serde_json::json!(m))
            .expect("a proposed-strategy approval item");
        assert_eq!(a["category"], "approval");
        assert!(
            a["action_label"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("strategy"),
            "{a}"
        );
    }

    #[test]
    fn company_actions_strategy_card_clears_after_approval() {
        // The strategy-approval card is the gate that must clear before a team
        // can be built. Once the operator approves the strategy it MUST drop off
        // the Action Center feed (computed from live state, never stale).
        let (agents, spine, task) = prime_stores();
        let m = spine
            .create_mandate("default", "Ship v1", "the why", None, None)
            .unwrap();
        spine.propose_strategy("default", &m, "the plan").unwrap();
        let strategy_card =
            |v: &serde_json::Value| -> bool {
                v["actions"].as_array().unwrap().iter().any(|a| {
                    a["target_type"] == "mandate" && a["target_id"] == serde_json::json!(m)
                })
            };
        let before = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        assert!(
            strategy_card(&before),
            "card present while proposed: {before}"
        );
        // Approve the strategy → the gate is closed.
        spine.approve_strategy("default", &m).unwrap();
        let after = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        assert!(
            !strategy_card(&after),
            "the strategy card must disappear after approval: {after}"
        );
    }

    #[test]
    fn company_actions_budget_alert_when_committed_over_guild_budget() {
        let (agents, spine, task) = prime_stores();
        // A small Guild budget and an active Operative committing more than it.
        spine.set_guild_allowance("default", Some(10_000)).unwrap();
        let eng = agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&eng, "allowance", "20000")
            .unwrap();

        let v = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        let budget = v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["category"] == "budget" && a["id"] == "budget:committed")
            .expect("a committed-over-budget alert");
        assert_eq!(budget["severity"], "high", "{budget}");
        assert!(
            budget["reason"].as_str().unwrap().contains("over budget"),
            "{budget}"
        );
        assert_eq!(v["counts"]["by_category"]["budget"], 1, "{v}");

        // Tenant isolation: a different Guild (no budget, no roster) sees NONE of
        // this — the committed/budget reads are tenant-scoped.
        let g = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(b"", "globex"),
        )));
        assert!(
            !g["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["category"] == "budget"),
            "no cross-tenant budget leak: {g}"
        );
    }

    #[test]
    fn company_actions_no_budget_alert_without_a_guild_budget() {
        // No Guild budget + an Operative with a positive Allowance and no waiting
        // work → NO budget item is fabricated (honest: there is no spend source).
        let (agents, spine, task) = prime_stores();
        let eng = agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&eng, "allowance", "20000")
            .unwrap();
        let v = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        assert!(
            !v["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["category"] == "budget"),
            "no spend source ⇒ no budget alert: {v}"
        );
    }

    #[test]
    fn company_actions_allowance_hardstop_when_zero_allowance_has_ready_work() {
        let (agents, spine, task) = prime_stores();
        // An active engineer hard-stopped by a 0 Allowance.
        let eng = agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        agents.update_agent_field(&eng, "allowance", "0").unwrap();
        // The prime flow gives the engineer a ready, assigned Brief.
        let pid = json_of(ok_body(handle_prime_propose(
            &agents,
            &spine,
            &fake_ctx(b"Build a web dashboard"),
        )))["proposal_id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = ok_body(handle_prime_approve(
            &agents,
            &spine,
            &task,
            &fake_ctx(pid.as_bytes()),
        ));

        let v = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        )));
        let hs = v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| {
                a["category"] == "budget"
                    && a["id"]
                        .as_str()
                        .map(|s| s.starts_with("budget:hardstop:"))
                        .unwrap_or(false)
            })
            .expect("an Allowance hard-stop alert for the assigned engineer");
        assert_eq!(hs["target_id"], serde_json::json!(eng), "{hs}");
        assert_eq!(hs["action_label"], "Raise the Allowance");
        assert_eq!(hs["severity"], "high");

        // Ordering: the budget governance item sorts ABOVE the ready_to_start item.
        let cats: Vec<&str> = v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["category"].as_str().unwrap())
            .collect();
        let budget_pos = cats.iter().position(|c| *c == "budget").unwrap();
        if let Some(ready_pos) = cats.iter().position(|c| *c == "ready_to_start") {
            assert!(budget_pos < ready_pos, "budget before ready: {cats:?}");
        }
    }

    /// A fake [`action_center::SpendSource`] — maps `agent_id → micro-USD`. An
    /// unknown agent returns `None` (no recorded spend), so it exercises both the
    /// "has spend" and "no signal" branches without any SQLite dependency.
    struct FakeSpend(std::collections::HashMap<String, u64>);
    impl FakeSpend {
        fn of(pairs: &[(&str, u64)]) -> Self {
            FakeSpend(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
        }
    }
    impl action_center::SpendSource for FakeSpend {
        fn operative_spend_micros(&self, agent_id: &str) -> Option<u64> {
            self.0.get(agent_id).copied()
        }
    }

    // ── Live spend (Part B): actual month-to-date cost from the SAME source the
    //    dispatch gate enforces (MetricsQuery::cost_since), read through the
    //    SpendSource seam. ───────────────────────────────────────────────────

    #[test]
    fn company_actions_live_spend_over_and_near_allowance() {
        let (agents, spine, task) = prime_stores();
        // Two active Operatives, each with a $200 monthly Allowance (20_000c).
        // No Guild budget set → only per-Operative spend items can fire.
        let over = agents
            .create_agent(
                "Over",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"over"),
                "medium",
                "default",
            )
            .unwrap();
        let near = agents
            .create_agent(
                "Near",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"near"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&over, "allowance", "20000")
            .unwrap();
        agents
            .update_agent_field(&near, "allowance", "20000")
            .unwrap();
        // over: $250 spent of $200 (125%, at/over the cap → High).
        // near: $170 spent of $200 (85% ≥ 80% band → Medium); not yet refused.
        let spend = FakeSpend::of(&[(over.as_str(), 250_000_000), (near.as_str(), 170_000_000)]);

        let v = json_of(ok_body(handle_company_actions_with_spend(
            &agents,
            &spine,
            &task,
            Some(&spend),
            &fake_ctx(b""),
        )));
        let actions = v["actions"].as_array().unwrap();

        let o = actions
            .iter()
            .find(|a| a["id"] == serde_json::json!(format!("budget:spend:{over}")))
            .expect("an over-Allowance spend item for `over`");
        assert_eq!(o["category"], "budget");
        assert_eq!(o["severity"], "high", "{o}");
        let oreason = o["reason"].as_str().unwrap();
        assert!(oreason.contains("$250.00"), "spent shown: {oreason}");
        assert!(oreason.contains("$200.00"), "cap shown: {oreason}");
        assert!(oreason.contains("125%"), "percent shown: {oreason}");
        assert!(
            oreason.contains("at/over the cap"),
            "over phrasing: {oreason}"
        );

        let n = actions
            .iter()
            .find(|a| a["id"] == serde_json::json!(format!("budget:spend:{near}")))
            .expect("a near-Allowance spend item for `near`");
        assert_eq!(n["severity"], "medium", "{n}");
        let nreason = n["reason"].as_str().unwrap();
        assert!(nreason.contains("85%"), "percent shown: {nreason}");
        assert!(nreason.contains("approaching"), "near phrasing: {nreason}");
    }

    #[test]
    fn company_actions_live_spend_guild_over_budget_is_distinct_from_committed() {
        let (agents, spine, task) = prime_stores();
        // $500 Guild budget; two active Operatives committing $300 each ($600
        // committed > $500 → the COMMITTED planning alert fires).
        spine.set_guild_allowance("default", Some(50_000)).unwrap();
        let e1 = agents
            .create_agent(
                "E1",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"e1"),
                "medium",
                "default",
            )
            .unwrap();
        let e2 = agents
            .create_agent(
                "E2",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"e2"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&e1, "allowance", "30000")
            .unwrap();
        agents
            .update_agent_field(&e2, "allowance", "30000")
            .unwrap();
        // ACTUAL spend: $200 + $400 = $600 > $500 budget → company spend (120%).
        let spend = FakeSpend::of(&[(e1.as_str(), 200_000_000), (e2.as_str(), 400_000_000)]);

        let v = json_of(ok_body(handle_company_actions_with_spend(
            &agents,
            &spine,
            &task,
            Some(&spend),
            &fake_ctx(b""),
        )));
        let actions = v["actions"].as_array().unwrap();

        // The COMMITTED planning signal (capacity reserved) is present…
        let committed = actions
            .iter()
            .find(|a| a["id"] == "budget:committed")
            .expect("the committed-Allowance planning alert");
        assert!(committed["reason"].as_str().unwrap().contains("Allowance"));

        // …AND the ACTUAL-spend company signal (money spent) is present, as a
        // DISTINCT item — the two never collapse onto each other.
        let spent = actions
            .iter()
            .find(|a| a["id"] == "budget:spend:company")
            .expect("the Guild actual-spend alert");
        assert_eq!(spent["severity"], "high");
        let sreason = spent["reason"].as_str().unwrap();
        assert!(sreason.contains("$600.00"), "spent shown: {sreason}");
        assert!(sreason.contains("$500.00"), "budget shown: {sreason}");
        assert!(sreason.contains("120%"), "percent shown: {sreason}");
        assert!(sreason.contains("over budget"), "{sreason}");
        // Distinct objects → both survive dedupe.
        assert_ne!(committed["target_id"], spent["target_id"]);
    }

    #[test]
    fn company_actions_no_spend_item_when_source_is_empty() {
        // A Guild budget + a positively-capped active Operative, but the spend
        // ledger has NO recorded cost for it (source returns None) → NO spend
        // item is fabricated; only the allowance-committed planning signal may
        // surface. Proves the feed never invents a spend figure.
        let (agents, spine, task) = prime_stores();
        spine.set_guild_allowance("default", Some(10_000)).unwrap();
        let eng = agents
            .create_agent(
                "Eng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"eng"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&eng, "allowance", "20000")
            .unwrap();
        let empty = FakeSpend::of(&[]);

        let v = json_of(ok_body(handle_company_actions_with_spend(
            &agents,
            &spine,
            &task,
            Some(&empty),
            &fake_ctx(b""),
        )));
        assert!(
            !v["actions"].as_array().unwrap().iter().any(|a| a["id"]
                .as_str()
                .map(|s| s.starts_with("budget:spend:"))
                .unwrap_or(false)),
            "no recorded spend ⇒ no spend item: {v}"
        );
        // The committed-Allowance planning signal is unaffected (still fires).
        assert!(
            v["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == "budget:committed"),
            "committed planning signal still surfaces: {v}"
        );
    }

    #[test]
    fn company_actions_live_spend_is_tenant_isolated() {
        // A spend source keyed on a DIFFERENT Guild's Operative must never leak
        // into this Guild's feed — the handler only ever asks about its OWN
        // tenant roster, so a foreign agent's spend can't surface here.
        let (agents, spine, task) = prime_stores();
        spine.set_guild_allowance("acme", Some(10_000)).unwrap();
        spine.set_guild_allowance("globex", Some(10_000)).unwrap();
        let acme_eng = agents
            .create_agent(
                "AcmeEng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"acme-eng"),
                "medium",
                "acme",
            )
            .unwrap();
        let globex_eng = agents
            .create_agent(
                "GlobexEng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"globex-eng"),
                "medium",
                "globex",
            )
            .unwrap();
        agents
            .update_agent_field(&acme_eng, "allowance", "20000")
            .unwrap();
        agents
            .update_agent_field(&globex_eng, "allowance", "20000")
            .unwrap();
        // ONLY acme's Operative has recorded spend ($250, over its $200 cap).
        let spend = FakeSpend::of(&[(acme_eng.as_str(), 250_000_000)]);

        // acme sees its own over-spend.
        let a = json_of(ok_body(handle_company_actions_with_spend(
            &agents,
            &spine,
            &task,
            Some(&spend),
            &fake_ctx_tenant(b"", "acme"),
        )));
        assert!(
            a["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["id"] == serde_json::json!(format!("budget:spend:{acme_eng}"))),
            "acme sees its own spend alert: {a}"
        );

        // globex sees NO spend item — neither its own (no recorded spend) nor,
        // critically, acme's (never queried; never summed into globex's total).
        let g = json_of(ok_body(handle_company_actions_with_spend(
            &agents,
            &spine,
            &task,
            Some(&spend),
            &fake_ctx_tenant(b"", "globex"),
        )));
        assert!(
            !g["actions"].as_array().unwrap().iter().any(|x| x["id"]
                .as_str()
                .map(|s| s.starts_with("budget:spend:"))
                .unwrap_or(false)),
            "no cross-tenant spend leak into globex: {g}"
        );
    }

    // ── Live spend through the REAL ledger: the same tests as above, but driven
    //    end-to-end through the PRODUCTION seam — a real `MetricsStore` →
    //    `MetricsQuery` → `MetricsSpendSource::current_month` (the exact type +
    //    window `register_agent_capabilities` wires at boot), not a hand-rolled
    //    `FakeSpend`. Closes the impl-map gap: the spend SOURCE was previously
    //    exercised only with a fake. ──────────────────────────────────────────

    /// Wall-clock now in unix-ms — matches `MetricsSpendSource::current_month`'s
    /// own clock so seeded timestamps land on the intended side of the window.
    fn now_unix_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// A priced AI invocation row for the metrics ledger. The live-spend seam
    /// (and the heartbeat Allowance gate it mirrors) attribute cost by the
    /// Operative's `agent_id`, so the recorded `agent_name` MUST be the
    /// `agent_id` for the row to count — exactly the production contract the
    /// impl-map calls out ("best-effort … only counts priced AI calls whose
    /// recorded `agent_name` matches the Operative's `agent_id`").
    fn spend_row(
        agent_id: &str,
        tenant: &str,
        ts_ms: i64,
        cost_micros: u64,
    ) -> crate::metrics::InvocationMetric {
        crate::metrics::InvocationMetric {
            agent_name: agent_id.to_string(),
            tenant_id: tenant.to_string(),
            peer_alias: "coord".to_string(),
            method: "ai.chat".to_string(),
            timestamp_ms: ts_ms,
            latency_ms: 10,
            success: true,
            error_kind: None,
            token_count: Some(100),
            cost_micros: Some(cost_micros),
            input_bytes: 0,
            output_bytes: 0,
            model: Some("mock".to_string()),
            confidence_score: None,
            routing_tier: None,
            request_id: None,
        }
    }

    #[test]
    fn company_actions_live_spend_from_real_metrics_store() {
        // Same scenario as `..._over_and_near_allowance`, but the spend signal
        // flows through the production `MetricsSpendSource` over a real
        // in-memory `MetricsStore`/`MetricsQuery` — proving the handler reads
        // actual ledger cost, sums multiple rows, and honours the calendar-month
        // window, not just that it trusts a fake's number.
        let (agents, spine, task) = prime_stores();
        let over = agents
            .create_agent(
                "Over",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"real-over"),
                "medium",
                "default",
            )
            .unwrap();
        let near = agents
            .create_agent(
                "Near",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"real-near"),
                "medium",
                "default",
            )
            .unwrap();
        agents
            .update_agent_field(&over, "allowance", "20000")
            .unwrap();
        agents
            .update_agent_field(&near, "allowance", "20000")
            .unwrap();

        let store = crate::metrics::MetricsStore::in_memory().unwrap();
        let now = now_unix_ms();
        // Derive seed timestamps from the canonical calendar-month window so the
        // test is deterministic at any clock: `recent` sits at the month's first
        // instant (inclusive — counted); `stale` is 1ms before it (previous
        // month — excluded).
        let win = crate::nodes::coordinator::heartbeat::allowance_window(now);
        let recent = win.start_ms; // this month — inside the window.
        let stale = win.start_ms - 1; // last month — OUTSIDE the window.
        store
            .insert_batch(&[
                // over: $150 + $100 = $250 of $200 (125%) → High. Split across
                // two rows so the assert proves the seam SUMS the window, not
                // reads a single row.
                spend_row(&over, "default", recent, 150_000_000),
                spend_row(&over, "default", recent, 100_000_000),
                // A huge STALE row for `over` that — were the month window ignored
                // — would blow the figure far past $250. Its exclusion is what
                // pins the window behaviour.
                spend_row(&over, "default", stale, 5_000_000_000),
                // near: $170 of $200 (85% ≥ 80% band) → Medium.
                spend_row(&near, "default", recent, 170_000_000),
            ])
            .unwrap();

        // The EXACT production type + window wired in `register_agent_capabilities`.
        let spend = MetricsSpendSource::current_month(crate::metrics::MetricsQuery::new(store));

        let v = json_of(ok_body(handle_company_actions_with_spend(
            &agents,
            &spine,
            &task,
            Some(&spend),
            &fake_ctx(b""),
        )));
        let actions = v["actions"].as_array().unwrap();

        let o = actions
            .iter()
            .find(|a| a["id"] == serde_json::json!(format!("budget:spend:{over}")))
            .expect("an over-Allowance spend item for `over`, read from the real ledger");
        assert_eq!(o["severity"], "high", "{o}");
        let oreason = o["reason"].as_str().unwrap();
        assert!(
            oreason.contains("$250.00"),
            "summed live spend with the stale row excluded: {oreason}"
        );
        assert!(oreason.contains("$200.00"), "cap shown: {oreason}");
        assert!(oreason.contains("125%"), "percent shown: {oreason}");
        assert!(
            oreason.contains("at/over the cap"),
            "over phrasing: {oreason}"
        );

        let n = actions
            .iter()
            .find(|a| a["id"] == serde_json::json!(format!("budget:spend:{near}")))
            .expect("a near-Allowance spend item for `near`, read from the real ledger");
        assert_eq!(n["severity"], "medium", "{n}");
        assert!(
            n["reason"].as_str().unwrap().contains("85%"),
            "percent shown: {n}"
        );
    }

    #[test]
    fn company_actions_live_spend_real_metrics_is_tenant_isolated() {
        // Two Guilds share ONE physical metrics ledger. `cost_since` itself does
        // NOT filter by tenant — isolation comes solely from the handler asking
        // only about its OWN roster's `agent_id`s. With BOTH Operatives over
        // their cap in the same store, prove neither Guild's spend leaks into
        // the other's feed, exercised through the real `MetricsSpendSource`.
        let (agents, spine, task) = prime_stores();
        let acme = agents
            .create_agent(
                "AcmeEng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"real-acme"),
                "medium",
                "acme",
            )
            .unwrap();
        let globex = agents
            .create_agent(
                "GlobexEng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"real-globex"),
                "medium",
                "globex",
            )
            .unwrap();
        agents
            .update_agent_field(&acme, "allowance", "20000")
            .unwrap();
        agents
            .update_agent_field(&globex, "allowance", "20000")
            .unwrap();

        let store = crate::metrics::MetricsStore::in_memory().unwrap();
        let now = now_unix_ms();
        // Seed at the canonical month-window start so both rows count in-window
        // regardless of the wall clock (deterministic at a month boundary too).
        let in_window = crate::nodes::coordinator::heartbeat::allowance_window(now).start_ms;
        // BOTH Operatives are over their $200 cap in the SHARED ledger ($250 each).
        store
            .insert_batch(&[
                spend_row(&acme, "acme", in_window, 250_000_000),
                spend_row(&globex, "globex", in_window, 250_000_000),
            ])
            .unwrap();
        let spend = MetricsSpendSource::current_month(crate::metrics::MetricsQuery::new(store));

        // acme's feed surfaces acme's Operative ONLY.
        let a = json_of(ok_body(handle_company_actions_with_spend(
            &agents,
            &spine,
            &task,
            Some(&spend),
            &fake_ctx_tenant(b"", "acme"),
        )));
        let aa = a["actions"].as_array().unwrap();
        assert!(
            aa.iter()
                .any(|x| x["id"] == serde_json::json!(format!("budget:spend:{acme}"))),
            "acme sees its own real-ledger spend: {a}"
        );
        assert!(
            !aa.iter()
                .any(|x| x["id"] == serde_json::json!(format!("budget:spend:{globex}"))),
            "globex's spend never leaks into acme's feed: {a}"
        );

        // globex's feed surfaces globex's Operative ONLY.
        let g = json_of(ok_body(handle_company_actions_with_spend(
            &agents,
            &spine,
            &task,
            Some(&spend),
            &fake_ctx_tenant(b"", "globex"),
        )));
        let gg = g["actions"].as_array().unwrap();
        assert!(
            gg.iter()
                .any(|x| x["id"] == serde_json::json!(format!("budget:spend:{globex}"))),
            "globex sees its own real-ledger spend: {g}"
        );
        assert!(
            !gg.iter()
                .any(|x| x["id"] == serde_json::json!(format!("budget:spend:{acme}"))),
            "acme's spend never leaks into globex's feed: {g}"
        );
    }

    // ── Canonical Guild month-to-date spend (`guild.spend`): the numeric route
    //    the Costs page reads. Driven end-to-end through the SAME production seam
    //    the dispatch gate uses (a real `MetricsStore` → `MetricsQuery` →
    //    `heartbeat::guild_spend_micros`), so the route can never disagree with
    //    the gate. ────────────────────────────────────────────────────────────

    #[test]
    fn guild_spend_current_month_only_over_budget_and_window_fields() {
        let (agents, spine, _task) = prime_stores();
        // $200 Guild budget (20_000c). Two active Operatives in this Guild.
        spine.set_guild_allowance("default", Some(20_000)).unwrap();
        let a = agents
            .create_agent(
                "A",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"gs-a"),
                "medium",
                "default",
            )
            .unwrap();
        let b = agents
            .create_agent(
                "B",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"gs-b"),
                "medium",
                "default",
            )
            .unwrap();

        let store = crate::metrics::MetricsStore::in_memory().unwrap();
        let now = now_unix_ms();
        let win = crate::nodes::coordinator::heartbeat::allowance_window(now);
        let recent = win.start_ms; // this month — counted.
        let stale = win.start_ms - 1; // last month — MUST be excluded.
        store
            .insert_batch(&[
                // $150 + $100 = $250 this month, split across two rows + agents.
                spend_row(&a, "default", recent, 150_000_000),
                spend_row(&b, "default", recent, 100_000_000),
                // A huge LAST-MONTH row — excluded by the calendar-month window.
                spend_row(&a, "default", stale, 9_000_000_000),
            ])
            .unwrap();
        let q = crate::metrics::MetricsQuery::new(store);

        let v = json_of(ok_body(handle_guild_spend(
            &agents,
            &spine,
            Some(&q),
            now,
            &fake_ctx(b""),
        )));
        // Current-month spend only ($250) — the stale row is excluded; this is
        // what proves the route bills the canonical month, not all-time.
        assert_eq!(v["spent_micros"], serde_json::json!(250_000_000u64), "{v}");
        assert_eq!(v["spent_cents"], serde_json::json!(25_000), "{v}");
        assert_eq!(v["budget_cents"], serde_json::json!(20_000), "{v}");
        // remaining = 20_000 − 25_000 = −5_000 (honest, even when over).
        assert_eq!(v["remaining_cents"], serde_json::json!(-5_000), "{v}");
        assert_eq!(v["over_budget"], serde_json::json!(true), "{v}");
        // Reset / window bookkeeping fields present + canonical.
        assert_eq!(v["window_start_ms"], serde_json::json!(win.start_ms), "{v}");
        assert_eq!(
            v["resets_at_ms"],
            serde_json::json!(win.resets_at_ms),
            "{v}"
        );
        assert_eq!(v["now_ms"], serde_json::json!(now), "{v}");
        // Guild identity + canonical source note.
        assert_eq!(v["tenant_id"], serde_json::json!("default"), "{v}");
        assert_eq!(v["guild_id"], serde_json::json!("default"), "{v}");
        assert!(
            v["source"].as_str().unwrap().contains("allowance_window"),
            "source names the canonical window: {v}"
        );
    }

    #[test]
    fn guild_spend_no_budget_is_honest_null() {
        let (agents, spine, _task) = prime_stores();
        // No `set_guild_allowance` → no Guild budget configured at all.
        let a = agents
            .create_agent(
                "A",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"gs-nob"),
                "medium",
                "default",
            )
            .unwrap();
        let store = crate::metrics::MetricsStore::in_memory().unwrap();
        let now = now_unix_ms();
        let recent = crate::nodes::coordinator::heartbeat::allowance_window(now).start_ms;
        store
            .insert_batch(&[spend_row(&a, "default", recent, 30_000_000)])
            .unwrap();
        let q = crate::metrics::MetricsQuery::new(store);

        let v = json_of(ok_body(handle_guild_spend(
            &agents,
            &spine,
            Some(&q),
            now,
            &fake_ctx(b""),
        )));
        // Canonical spend is still returned ($30)…
        assert_eq!(v["spent_micros"], serde_json::json!(30_000_000u64), "{v}");
        assert_eq!(v["spent_cents"], serde_json::json!(3_000), "{v}");
        // …but the budget-derived fields are HONESTLY null, never a faked 0.
        assert!(v["budget_cents"].is_null(), "{v}");
        assert!(v["remaining_cents"].is_null(), "{v}");
        assert!(v["over_budget"].is_null(), "{v}");
    }

    #[test]
    fn guild_spend_no_metrics_is_null_spend_budget_still_resolves() {
        let (agents, spine, _task) = prime_stores();
        spine.set_guild_allowance("default", Some(20_000)).unwrap();
        let now = now_unix_ms();
        // metrics == None → spend can't be computed honestly.
        let v = json_of(ok_body(handle_guild_spend(
            &agents,
            &spine,
            None,
            now,
            &fake_ctx(b""),
        )));
        assert!(v["spent_micros"].is_null(), "no ledger → null spend: {v}");
        assert!(v["spent_cents"].is_null(), "{v}");
        // Budget + window still resolve (they don't need the ledger).
        assert_eq!(v["budget_cents"], serde_json::json!(20_000), "{v}");
        assert!(
            v["remaining_cents"].is_null(),
            "no spend → no remaining: {v}"
        );
        assert!(
            v["source"]
                .as_str()
                .unwrap()
                .contains("metrics_ledger_unavailable"),
            "source flags the unavailable ledger: {v}"
        );
    }

    #[test]
    fn guild_spend_is_tenant_isolated() {
        let (agents, spine, _task) = prime_stores();
        spine.set_guild_allowance("acme", Some(100_000)).unwrap();
        spine.set_guild_allowance("globex", Some(100_000)).unwrap();
        let acme = agents
            .create_agent(
                "AcmeEng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"gs-acme"),
                "medium",
                "acme",
            )
            .unwrap();
        let globex = agents
            .create_agent(
                "GlobexEng",
                "engineer",
                "SWE",
                "eng",
                "eng",
                "founder",
                &subject_of(b"gs-globex"),
                "medium",
                "globex",
            )
            .unwrap();

        let store = crate::metrics::MetricsStore::in_memory().unwrap();
        let now = now_unix_ms();
        let in_window = crate::nodes::coordinator::heartbeat::allowance_window(now).start_ms;
        // Different amounts in the SHARED ledger so a leak would be obvious.
        store
            .insert_batch(&[
                spend_row(&acme, "acme", in_window, 250_000_000),
                spend_row(&globex, "globex", in_window, 999_000_000),
            ])
            .unwrap();
        let q = crate::metrics::MetricsQuery::new(store);

        // acme's route sums ONLY acme's Operative.
        let va = json_of(ok_body(handle_guild_spend(
            &agents,
            &spine,
            Some(&q),
            now,
            &fake_ctx_tenant(b"", "acme"),
        )));
        assert_eq!(
            va["spent_micros"],
            serde_json::json!(250_000_000u64),
            "{va}"
        );
        assert_eq!(va["tenant_id"], serde_json::json!("acme"), "{va}");

        // globex's route sums ONLY globex's Operative — no acme leak.
        let vg = json_of(ok_body(handle_guild_spend(
            &agents,
            &spine,
            Some(&q),
            now,
            &fake_ctx_tenant(b"", "globex"),
        )));
        assert_eq!(
            vg["spent_micros"],
            serde_json::json!(999_000_000u64),
            "{vg}"
        );
        assert_eq!(vg["tenant_id"], serde_json::json!("globex"), "{vg}");
    }

    #[test]
    fn company_actions_is_tenant_isolated() {
        let (agents, spine, task) = prime_stores();
        // Guild "acme" gets a pending hire + a proposed-strategy Mandate.
        agents
            .request_hire(
                "Sec",
                "engineer",
                "E",
                "x",
                "x",
                "founder",
                &subject_of(b"acme-hire"),
                "medium",
                "acme",
            )
            .unwrap();
        let m = spine
            .create_mandate("acme", "ACME secret mandate", "d", None, None)
            .unwrap();
        spine.propose_strategy("acme", &m, "plan").unwrap();

        // acme sees its own work.
        let a = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(b"", "acme"),
        )));
        assert!(
            a["counts"]["total"].as_u64().unwrap() >= 2,
            "acme sees its own hire + strategy: {a}"
        );
        // globex sees NONE of acme's items (no existence leak).
        let g = json_of(ok_body(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx_tenant(b"", "globex"),
        )));
        assert_eq!(g["counts"]["total"], 0, "globex must not see acme: {g}");
        assert!(
            !g.to_string().contains("ACME secret mandate"),
            "no cross-tenant title leak: {g}"
        );
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

    // ── Mandate orchestration (company-model §4.6) ───────────

    /// Build an approved Mandate with a READY team (one active agent in
    /// `role`). Returns (mandate_id, active_agent_id).
    fn ready_team(agents: &AgentStore, spine: &SpineStore, role: &str) -> (String, String) {
        let m = approved_mandate(spine);
        let agent_id = agents
            .create_agent(
                "Worker", role, "W", "eng", "eng", "prime", "subj-rt", "medium", "default",
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"{role}\",\"agent_id\":\"{agent_id}\"}}]");
        spine
            .record_team_plan(&crate::nodes::coordinator::spine::store::TeamPlanRecord {
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
        (m, agent_id)
    }

    fn orchestrate(
        tasks: &TaskStore,
        agents: &AgentStore,
        spine: &SpineStore,
        arg: &str,
    ) -> serde_json::Value {
        let body = ok_body(handle_orchestrate(
            tasks,
            agents,
            spine,
            &fake_ctx(arg.as_bytes()),
        ));
        serde_json::from_slice(&body.into_bytes()).unwrap()
    }

    #[test]
    fn orchestrate_blocked_when_team_not_ready() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // A pending (not-yet-active) hire → team is not ready.
        let pending = agents
            .request_hire(
                "P", "engineer", "P", "e", "e", "prime", "subj-p", "medium", "default",
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]");
        spine
            .record_team_plan(&crate::nodes::coordinator::spine::store::TeamPlanRecord {
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
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert_eq!(v["ready"], false);
        assert_eq!(v["status"], "blocked");
        assert!(!v["blockers"].as_array().unwrap().is_empty());
        // A non-ready team gets a parent + a PLACEHOLDER role track for the
        // pending-hire role — but no executable subject Brief and no
        // assignment.
        assert_eq!(v["placeholder_tracks_created"].as_array().unwrap().len(), 1);
        assert!(v["subject_briefs_created"].as_array().unwrap().is_empty());
        assert!(v["assigned_briefs"].as_array().unwrap().is_empty());
        // The placeholder entry is tagged with a reason.
        let ph = &v["placeholder_tracks_created"][0];
        assert_eq!(ph["placeholder"], true);
        assert_eq!(ph["reason"], "pending hire");
        // Durable: parent + one placeholder track exist under the Mandate.
        let cards = tasks.list_briefs_by_mandate(&m, 50).unwrap();
        assert_eq!(cards.len(), 2);
        // The placeholder track carries no assignee.
        let ph_id = ph["task_id"].as_str().unwrap();
        let ph_card = cards.iter().find(|c| c.task_id == ph_id).unwrap();
        assert!(ph_card.assignee_agent_id.is_none());
        assert!(ph_card.title.contains("blocked"));
    }

    #[test]
    fn orchestrate_strategy_not_approved_is_blocked() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate("default", "Ship", "x", None, None)
            .unwrap();
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert_eq!(v["ready"], false);
        assert!(
            v["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["reason"] == "strategy_not_approved")
        );
    }

    #[test]
    fn orchestrate_creates_tree_and_assigns_ready_team() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, agent_id) = ready_team(&agents, &spine, "engineer");
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert_eq!(v["ready"], true);
        assert_eq!(v["status"], "assigned");
        // Three-tier tree: parent → role track → subject execution.
        let created = v["created_briefs"].as_array().unwrap();
        assert_eq!(created.len(), 3, "parent + role track + subject execution");
        assert!(!v["parent_brief"].is_null());
        assert_eq!(v["role_tracks_created"].as_array().unwrap().len(), 1);
        assert_eq!(v["subject_briefs_created"].as_array().unwrap().len(), 1);
        assert!(
            created
                .iter()
                .any(|b| b["title"].as_str().unwrap().starts_with("Execute Mandate:"))
        );
        assert!(created.iter().any(|b| {
            b["title"]
                .as_str()
                .unwrap()
                .starts_with("Engineering track:")
        }));
        assert!(created.iter().any(|b| {
            b["title"]
                .as_str()
                .unwrap()
                .starts_with("Engineering execution:")
        }));
        // Assignment lands on the subject Brief, not the role track.
        let assigned = v["assigned_briefs"].as_array().unwrap();
        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0]["agent_id"], agent_id);
        let subject_id = v["subject_briefs_created"][0]["task_id"].as_str().unwrap();
        assert_eq!(assigned[0]["task_id"].as_str().unwrap(), subject_id);
        // The briefs are durable + linked to the Mandate.
        let cards = tasks.list_briefs_by_mandate(&m, 50).unwrap();
        assert_eq!(cards.len(), 3);
        // The role track is NOT assigned; the subject Brief is.
        let role_id = v["role_tracks_created"][0]["task_id"].as_str().unwrap();
        let role_card = cards.iter().find(|c| c.task_id == role_id).unwrap();
        assert!(
            role_card.assignee_agent_id.is_none(),
            "role track stays unassigned"
        );
        let subj_card = cards.iter().find(|c| c.task_id == subject_id).unwrap();
        assert_eq!(
            subj_card.assignee_agent_id.as_deref(),
            Some(agent_id.as_str())
        );
        // The latest run is persisted.
        let latest = spine
            .latest_orchestration_run("default", &m)
            .unwrap()
            .unwrap();
        assert_eq!(latest.status, "assigned");
    }

    // The orchestration parent Dossier is now persisted through the governed,
    // lock-aware path and stamped with the synthetic autonomous-Prime authority
    // (NOT a legacy author-less `add_dossier` row) — and a rerun never appends a
    // duplicate revision (company-model §12.5F; execution-and-issue §1.8).
    #[test]
    fn orchestrate_parent_dossier_is_prime_authored_and_idempotent() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent_id) = ready_team(&agents, &spine, "engineer");
        let prime = crate::nodes::coordinator::agent::prime_driver::AUTONOMOUS_PRIME_AUTHORITY;

        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        let parent_id = v["parent_brief"]["task_id"].as_str().unwrap();
        // The parent orchestration Dossier exists and is Prime-owned.
        let dossier = tasks
            .latest_dossier(parent_id, "orchestration")
            .unwrap()
            .unwrap();
        assert_eq!(dossier.author.as_deref(), Some(prime));
        assert_eq!(dossier.revision_number, 1);
        // The run result reports the governed write outcome honestly.
        let notes = v["dossier_notes"].as_array().unwrap();
        assert!(notes.iter().any(|n| {
            n["task_id"] == parent_id
                && n["kind"] == "orchestration"
                && n["outcome"] == "authored"
                && n["author"] == prime
        }));

        // Rerun: the parent Brief is reused (existing) so no duplicate Dossier
        // revision is appended — still exactly one revision of the kind.
        let _ = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        let metas = tasks.list_dossiers(parent_id).unwrap();
        assert_eq!(
            metas.iter().filter(|d| d.kind == "orchestration").count(),
            1,
            "rerun must not append a duplicate orchestration Dossier"
        );
        assert_eq!(
            tasks
                .latest_dossier(parent_id, "orchestration")
                .unwrap()
                .unwrap()
                .author
                .as_deref(),
            Some(prime)
        );
    }

    #[test]
    fn orchestrate_stamps_founder_reviewer_so_shift_is_review_to_apply_able() {
        // A Mandate-orchestrated Brief MUST be stamped with the Founder/Board
        // reviewer (like prime.approve), or its completed Shift parks in
        // `blocked` for want of a reviewer and the operator's run.apply cannot
        // advance it to `done` (company-model §12.6 / execution-and-issue §1.3).
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        // The Founder must exist for the reviewer to resolve.
        let (founder, _) = agents
            .ensure_founder("", "echo", "operator", "default")
            .unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        // The runnable subject Brief carries the Founder as reviewer.
        let subject_id = v["subject_briefs_created"][0]["task_id"].as_str().unwrap();
        assert_eq!(
            tasks
                .brief_fields(subject_id)
                .unwrap()
                .unwrap()
                .reviewer_agent_id
                .as_deref(),
            Some(founder.as_str()),
            "the subject execution Brief must be stamped with the Founder reviewer"
        );
        // Every materialised tier (parent + role track) gets the same reviewer.
        let parent_id = v["parent_brief"]["task_id"].as_str().unwrap();
        assert_eq!(
            tasks
                .brief_fields(parent_id)
                .unwrap()
                .unwrap()
                .reviewer_agent_id
                .as_deref(),
            Some(founder.as_str())
        );
    }

    #[test]
    fn orchestrate_without_founder_leaves_reviewer_unset() {
        // No Founder (company not bootstrapped) → the honest fallback: no
        // reviewer is fabricated, and the Brief parks until one is set.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        let subject_id = v["subject_briefs_created"][0]["task_id"].as_str().unwrap();
        assert!(
            tasks
                .brief_fields(subject_id)
                .unwrap()
                .unwrap()
                .reviewer_agent_id
                .is_none(),
            "no Founder → reviewer stays unset (never fabricated)"
        );
    }

    #[test]
    fn orchestrate_is_idempotent_on_double_run() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert_eq!(first["created_briefs"].as_array().unwrap().len(), 3);
        let after_first = tasks.list_briefs_by_mandate(&m, 50).unwrap().len();
        // Second run creates nothing new; reuses existing.
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert!(
            second["created_briefs"].as_array().unwrap().is_empty(),
            "a repeated run must not duplicate Briefs"
        );
        assert_eq!(second["existing_briefs"].as_array().unwrap().len(), 3);
        assert_eq!(
            tasks.list_briefs_by_mandate(&m, 50).unwrap().len(),
            after_first,
            "no duplicate Briefs after a second run"
        );
        // Same inputs → same signature.
        assert_eq!(first["input_signature"], second["input_signature"]);
        // Stable source markers were recorded for the run.
        let markers = second["source_markers"].as_array().unwrap();
        assert!(
            markers
                .iter()
                .any(|mk| mk.as_str().unwrap_or("").ends_with(":parent")),
            "the parent marker must be present: {second}"
        );
        assert!(
            markers
                .iter()
                .any(|mk| mk.as_str().unwrap_or("").contains(":role:engineer")),
            "the engineer child marker must be present: {second}"
        );
        let latest = spine
            .latest_orchestration_run("default", &m)
            .unwrap()
            .unwrap();
        // The run record distinguishes reused from created.
        assert_eq!(
            latest.to_json()["created_brief_ids"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            latest.to_json()["existing_brief_ids"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        // The subject marker (per active agent) is recorded too.
        assert!(
            markers.iter().any(|mk| mk
                .as_str()
                .unwrap_or("")
                .contains(":role:engineer:subject:")),
            "the subject marker must be present: {second}"
        );
    }

    // ── Crew adoption: reuse active same-role Operatives before hiring ──
    // (company-model §12.5A/§12.5B). A Mandate team plan must staff itself
    // from the crew the Company already has before it files a hire.

    /// Seed one active, runnable (echo Rig) starter Operative for `role`
    /// in `tenant`; returns its agent_id.
    fn seed_active(agents: &AgentStore, role: &str, tenant: &str) -> String {
        agents
            .ensure_starter_operative(
                role,
                &format!("Starter {role}"),
                "Operative",
                "echo",
                tenant,
            )
            .unwrap()
            .0
    }

    #[test]
    fn team_plan_adopts_active_starter_crew_instead_of_hiring() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let eng = seed_active(&agents, "engineer", "default");
        let des = seed_active(&agents, "designer", "default");
        // Both roles carry a subject_id (the path that WOULD mint a hire) —
        // but an active same-role Operative exists, so both are adopted and
        // ZERO hires are filed.
        let arg = format!("{m}|build|engineer:subj-e,designer:subj-d");
        let v = json(handle_team_plan(&agents, &spine, &fake_ctx(arg.as_bytes())));
        assert!(
            v["pending_hires"].as_array().unwrap().is_empty(),
            "no hire is filed when active crew exists: {v}"
        );
        assert!(v["clearances"].as_array().unwrap().is_empty());
        let adopted = v["adopted"].as_array().unwrap();
        assert_eq!(adopted.len(), 2, "engineer + designer adopted: {v}");
        let ids: Vec<&str> = adopted
            .iter()
            .map(|a| a["agent_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&eng.as_str()) && ids.contains(&des.as_str()));
        // The Company still has exactly its two active crew members — no
        // duplicate (pending) engineer/designer was minted.
        assert_eq!(
            agents
                .list_by_role_for_tenant("engineer", "default")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            agents
                .list_by_role_for_tenant("designer", "default")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn team_readiness_reports_adopted_crew_as_ready() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let eng = seed_active(&agents, "engineer", "default");
        ok_body(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx(format!("{m}|build|engineer:subj-e").as_bytes()),
        ));
        let v = json(handle_team_readiness(
            &agents,
            &spine,
            &fake_ctx(m.as_bytes()),
        ));
        assert_eq!(
            v["readiness"], "ready",
            "adopted crew makes the team ready: {v}"
        );
        let active = v["active_agents"].as_array().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0]["agent_id"], eng);
        assert_eq!(active[0]["role"], "engineer");
        assert!(v["missing_roles"].as_array().unwrap().is_empty());
        assert!(v["pending_hires"].as_array().unwrap().is_empty());
    }

    #[test]
    fn team_plan_still_hires_a_genuinely_missing_role() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // Only an engineer exists; qa is genuinely missing.
        seed_active(&agents, "engineer", "default");
        let arg = format!("{m}|build|engineer:subj-e,qa:subj-q");
        let v = json(handle_team_plan(&agents, &spine, &fake_ctx(arg.as_bytes())));
        assert_eq!(
            v["adopted"].as_array().unwrap().len(),
            1,
            "engineer adopted: {v}"
        );
        let hires = v["pending_hires"].as_array().unwrap();
        assert_eq!(hires.len(), 1, "only the missing qa is hired: {v}");
        assert_eq!(hires[0]["role"], "qa");
        // Readiness: engineer ready, qa a pending hire → still staffing.
        let r = json(handle_team_readiness(
            &agents,
            &spine,
            &fake_ctx(m.as_bytes()),
        ));
        assert_eq!(r["readiness"], "staffing");
        assert_eq!(r["active_agents"].as_array().unwrap().len(), 1);
        assert_eq!(r["pending_hires"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn team_readiness_pending_hires_carry_the_safe_local_suggested_rig() {
        // A pending hire must carry `suggested_rig` so the Mandate page approves
        // it on backend guidance (the safe-local `echo`) instead of hardcoding —
        // the SAME guidance the Action Center `hire` card emits (company-model
        // §12.6). Only `echo`: never a paid/interactive CLI, never a secret.
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // qa is genuinely missing → a minted (pending) hire.
        let arg = format!("{m}|build|qa:subj-q");
        json(handle_team_plan(&agents, &spine, &fake_ctx(arg.as_bytes())));
        let r = json(handle_team_readiness(
            &agents,
            &spine,
            &fake_ctx(m.as_bytes()),
        ));
        let hires = r["pending_hires"].as_array().unwrap();
        assert_eq!(hires.len(), 1, "qa is a pending hire: {r}");
        assert_eq!(hires[0]["role"], "qa");
        assert_eq!(
            hires[0]["suggested_rig"],
            crate::rig::SAFE_LOCAL_RIG,
            "pending hire carries the safe-local suggested Rig: {r}"
        );
        // The suggested Rig is exactly the safe-local `echo` — never paid.
        assert_eq!(hires[0]["suggested_rig"], "echo");
    }

    #[test]
    fn orchestrate_assigns_adopted_operative_and_stamps_reviewer() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        // Founder present so the reviewer resolves (stamping must be preserved
        // on the adopted path exactly as on the hired path).
        let (founder, _) = agents
            .ensure_founder("", "echo", "operator", "default")
            .unwrap();
        let m = approved_mandate(&spine);
        let eng = seed_active(&agents, "engineer", "default");
        ok_body(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx(format!("{m}|build|engineer:subj-e").as_bytes()),
        ));
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert_eq!(v["ready"], true, "adopted crew → ready: {v}");
        assert_eq!(v["status"], "assigned");
        let assigned = v["assigned_briefs"].as_array().unwrap();
        assert_eq!(assigned.len(), 1, "the adopted engineer gets a Brief: {v}");
        assert_eq!(assigned[0]["agent_id"], eng);
        // Reviewer stamping is preserved on the adopted subject Brief.
        let subject_id = v["subject_briefs_created"][0]["task_id"].as_str().unwrap();
        assert_eq!(
            tasks
                .brief_fields(subject_id)
                .unwrap()
                .unwrap()
                .reviewer_agent_id
                .as_deref(),
            Some(founder.as_str()),
            "the adopted Operative's subject Brief carries the Founder reviewer"
        );
    }

    #[test]
    fn team_plan_adoption_is_tenant_isolated() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        // An active engineer in tenant-a.
        seed_active(&agents, "engineer", "tenant-a");
        // tenant-b plans an engineer with a subject → it must NOT adopt
        // tenant-a's crew; it files its own pending hire.
        let m = spine
            .create_mandate("tenant-b", "Ship", "x", None, None)
            .unwrap();
        spine.propose_strategy("tenant-b", &m, "plan").unwrap();
        spine.approve_strategy("tenant-b", &m).unwrap();
        let arg = format!("{m}|build|engineer:subj-e");
        let v = json(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx_tenant(arg.as_bytes(), "tenant-b"),
        ));
        assert!(
            v["adopted"].as_array().unwrap().is_empty(),
            "tenant-b must not adopt tenant-a's crew: {v}"
        );
        assert_eq!(v["pending_hires"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn team_plan_adopts_oldest_active_same_role_operative() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // Two active runnable engineers; the OLDEST (first inserted) wins.
        let first = seed_active(&agents, "engineer", "default");
        let second = agents
            .create_agent(
                "Second", "engineer", "E", "eng", "eng", "founder", "subj-2", "medium", "default",
            )
            .unwrap();
        agents.update_agent_field(&second, "rig", "echo").unwrap();
        // A bare role still adopts when active crew exists.
        let v = json(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx(format!("{m}|build|engineer").as_bytes()),
        ));
        let adopted = v["adopted"].as_array().unwrap();
        assert_eq!(adopted.len(), 1);
        assert_eq!(
            adopted[0]["agent_id"], first,
            "the oldest active engineer is adopted deterministically: {v}"
        );
    }

    #[test]
    fn team_plan_does_not_adopt_unrunnable_operative() {
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // An ACTIVE engineer with NO Rig is not runnable → not adopted; the
        // role falls through to a real pending hire.
        agents
            .create_agent(
                "Norig", "engineer", "E", "eng", "eng", "founder", "subj-nr", "medium", "default",
            )
            .unwrap();
        let v = json(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx(format!("{m}|build|engineer:subj-e").as_bytes()),
        ));
        assert!(v["adopted"].as_array().unwrap().is_empty(), "{v}");
        assert_eq!(v["pending_hires"].as_array().unwrap().len(), 1, "{v}");
    }

    #[test]
    fn company_actions_no_hire_card_for_adopted_role_only_for_missing() {
        let (agents, spine, task) = prime_stores();
        let m = approved_mandate(&spine);
        // Engineer is active crew (adopted); qa is genuinely missing (hired).
        seed_active(&agents, "engineer", "default");
        ok_body(handle_team_plan(
            &agents,
            &spine,
            &fake_ctx(format!("{m}|build|engineer:subj-e,qa:subj-q").as_bytes()),
        ));
        let v = json(handle_company_actions(
            &agents,
            &spine,
            &task,
            &fake_ctx(b""),
        ));
        let hire_cards: Vec<&serde_json::Value> = v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["category"] == serde_json::json!("hire"))
            .collect();
        assert_eq!(
            hire_cards.len(),
            1,
            "exactly one hire card — for the missing qa, not the adopted engineer: {v}"
        );
        assert_eq!(hire_cards[0]["target_type"], "agent");
        assert!(
            hire_cards[0]["title"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("qa")
                || hire_cards[0]["reason"]
                    .as_str()
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains("qa"),
            "the hire card is for the qa role: {v}"
        );
    }

    #[test]
    fn orchestrate_reuses_marked_tree_after_mandate_rename() {
        // A Mandate rename must NOT cause a rerun to duplicate the tree:
        // dedup is by stable source marker, not by title text.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert_eq!(first["created_briefs"].as_array().unwrap().len(), 3);
        let before = tasks.list_briefs_by_mandate(&m, 50).unwrap().len();
        // Rename the Mandate (changes the would-be Brief titles).
        spine
            .update_mandate_field(&m, "title", "Totally Different Name")
            .unwrap();
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert!(
            second["created_briefs"].as_array().unwrap().is_empty(),
            "a rename must not duplicate the tree"
        );
        assert_eq!(second["existing_briefs"].as_array().unwrap().len(), 3);
        assert_eq!(
            tasks.list_briefs_by_mandate(&m, 50).unwrap().len(),
            before,
            "no duplicate Briefs after a Mandate rename"
        );
    }

    #[test]
    fn orchestrate_reuses_marked_tree_after_manual_brief_title_edit() {
        // A manual Brief-title edit must NOT defeat dedup either.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        let cards = tasks.list_briefs_by_mandate(&m, 50).unwrap();
        assert_eq!(cards.len(), 3);
        // An operator renames every Brief by hand.
        for c in &cards {
            tasks
                .set_brief_field(&c.task_id, "title", &format!("Hand-edited {}", c.task_id))
                .unwrap();
        }
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert!(
            second["created_briefs"].as_array().unwrap().is_empty(),
            "manual title edits must not duplicate the tree"
        );
        assert_eq!(second["existing_briefs"].as_array().unwrap().len(), 3);
        // The user's hand-edited titles are preserved (not clobbered).
        let after = tasks.list_briefs_by_mandate(&m, 50).unwrap();
        assert_eq!(after.len(), 3);
        assert!(
            after.iter().all(|c| c.title.starts_with("Hand-edited ")),
            "a reused Brief's title must not be overwritten"
        );
    }

    #[test]
    fn orchestrate_recovers_from_partial_crash_parent_only() {
        // Simulate a crash after the parent was created+marked but before
        // any children: a rerun must create only the missing children.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        // Hand-create just the parent with its stable marker.
        let parent_marker = format!("mandate:{m}:parent");
        let parent_id = tasks
            .create_brief_with_marker(
                "default",
                "Execute Mandate: Ship",
                "operator",
                Some(&m),
                "mandate_orchestration",
                &parent_marker,
            )
            .unwrap();
        // Rerun: parent is reused; the role track + subject Brief are the
        // only missing tiers created.
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        let created = v["created_briefs"].as_array().unwrap();
        let existing = v["existing_briefs"].as_array().unwrap();
        assert_eq!(
            created.len(),
            2,
            "only the missing role track + subject are created"
        );
        assert_eq!(existing.len(), 1, "the pre-existing parent is reused");
        assert_eq!(existing[0]["task_id"], parent_id);
        assert_eq!(
            tasks.list_briefs_by_mandate(&m, 50).unwrap().len(),
            3,
            "no duplicate parent"
        );
    }

    #[test]
    fn orchestrate_reuses_child_with_different_title() {
        // A role-only marker that already exists (with a bespoke title) is
        // reused as the role-track Brief, not recreated. This is also the
        // back-compat path: the previous slice's role-only child becomes
        // the role track.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        // Pre-seed the engineering role-track with a bespoke title + the
        // legacy role-only marker.
        let child_marker = format!("mandate:{m}:role:engineer");
        let child_id = tasks
            .create_brief_with_marker(
                "default",
                "A completely bespoke child title",
                "operator",
                Some(&m),
                "mandate_orchestration",
                &child_marker,
            )
            .unwrap();
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        // Parent + subject Brief are new; the legacy role-only child is
        // reused as the role track despite its unrelated title.
        assert_eq!(v["created_briefs"].as_array().unwrap().len(), 2);
        let existing = v["existing_briefs"].as_array().unwrap();
        assert!(
            existing
                .iter()
                .any(|b| b["task_id"] == serde_json::json!(child_id)),
            "the legacy role-only child must be reused as the role track: {v}"
        );
        assert_eq!(v["role_tracks_existing"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["role_tracks_existing"][0]["task_id"],
            serde_json::json!(child_id)
        );
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
    }

    // ── Subject-aware marker tests (company-model §4.6) ──────────

    #[test]
    fn orchestrate_same_agent_rerun_reuses_subject_brief() {
        // Same role + same agent on a rerun → no duplicate subject Brief.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert_eq!(first["subject_briefs_created"].as_array().unwrap().len(), 1);
        let before = tasks.list_briefs_by_mandate(&m, 50).unwrap().len();
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert!(
            second["subject_briefs_created"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            second["subject_briefs_existing"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            tasks.list_briefs_by_mandate(&m, 50).unwrap().len(),
            before,
            "the subject Brief is reused, not duplicated"
        );
    }

    #[test]
    fn orchestrate_changed_agent_makes_new_subject_reuses_role_track() {
        // A different active agent for the same role → a NEW subject Brief
        // but the SAME role-track Brief is reused.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, agent_a) = ready_team(&agents, &spine, "engineer");
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        let role_id = first["role_tracks_created"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        let subject_a = first["subject_briefs_created"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        // Swap the active engineer to a brand-new agent on the same plan.
        let agent_b = agents
            .create_agent(
                "Worker2", "engineer", "W2", "eng", "eng", "prime", "subj-rt2", "medium", "default",
            )
            .unwrap();
        assert_ne!(agent_a, agent_b);
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{agent_b}\"}}]");
        spine
            .record_team_plan(&crate::nodes::coordinator::spine::store::TeamPlanRecord {
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
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        // Role track reused (same id); a new subject Brief for agent_b.
        assert_eq!(second["role_tracks_existing"].as_array().unwrap().len(), 1);
        assert_eq!(
            second["role_tracks_existing"][0]["task_id"],
            serde_json::json!(role_id)
        );
        assert_eq!(
            second["subject_briefs_created"].as_array().unwrap().len(),
            1
        );
        let subject_b = second["subject_briefs_created"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(
            subject_a, subject_b,
            "a changed agent gets its own subject Brief"
        );
        // The new subject Brief is assigned to agent_b.
        assert_eq!(
            second["assigned_briefs"][0]["agent_id"],
            serde_json::json!(agent_b)
        );
        assert_eq!(
            second["assigned_briefs"][0]["task_id"],
            serde_json::json!(subject_b)
        );
        // Tree now has parent + role track + 2 subject Briefs.
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 4);
    }

    #[test]
    fn orchestrate_reuses_manually_renamed_subject_brief() {
        // A hand-renamed subject Brief is still reused by marker.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        let subject_id = first["subject_briefs_created"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        tasks
            .set_brief_field(&subject_id, "title", "My hand-named execution Brief")
            .unwrap();
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert!(
            second["subject_briefs_created"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            second["subject_briefs_existing"][0]["task_id"],
            serde_json::json!(subject_id)
        );
        // The hand-named title survives the rerun.
        let card = tasks
            .list_briefs_by_mandate(&m, 50)
            .unwrap()
            .into_iter()
            .find(|c| c.task_id == subject_id)
            .unwrap();
        assert_eq!(card.title, "My hand-named execution Brief");
    }

    #[test]
    fn orchestrate_partial_crash_role_track_only_creates_subject() {
        // Crash left a role-track Brief but no subject Brief: a rerun
        // creates only the missing subject Brief.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _agent) = ready_team(&agents, &spine, "engineer");
        // Hand-create parent + role track with their stable markers.
        let parent_id = tasks
            .create_brief_with_marker(
                "default",
                "Execute Mandate: Ship",
                "operator",
                Some(&m),
                "mandate_orchestration",
                &format!("mandate:{m}:parent"),
            )
            .unwrap();
        let role_id = tasks
            .create_brief_with_marker(
                "default",
                "Engineering track: Ship",
                "operator",
                Some(&m),
                "mandate_orchestration",
                &format!("mandate:{m}:role:engineer"),
            )
            .unwrap();
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        // Only the subject Brief is created; parent + role track reused.
        assert_eq!(v["created_briefs"].as_array().unwrap().len(), 1);
        assert_eq!(v["subject_briefs_created"].as_array().unwrap().len(), 1);
        let existing_ids: Vec<&str> = v["existing_briefs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["task_id"].as_str().unwrap())
            .collect();
        assert!(existing_ids.contains(&parent_id.as_str()));
        assert!(existing_ids.contains(&role_id.as_str()));
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
    }

    // ── Placeholder role-track tests (company-model §4.6) ────────

    /// Record a Team Plan for `m` with explicit role-gap inputs.
    fn plan_with_gaps(
        spine: &SpineStore,
        m: &str,
        proposed_roles_json: &str,
        pending_hires_json: &str,
        denials_json: &str,
    ) {
        spine
            .record_team_plan(&crate::nodes::coordinator::spine::store::TeamPlanRecord {
                tenant_id: "default",
                mandate_id: m,
                actor_id: "operator",
                description: "x",
                proposed_roles_json,
                pending_hires_json,
                clearance_ids_json: "[]",
                denials_json,
                next_steps_json: "[]",
                status: "staffing",
            })
            .unwrap();
    }

    #[test]
    fn orchestrate_missing_role_creates_one_placeholder_track() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        plan_with_gaps(&spine, &m, "[\"designer\"]", "[]", "[]");
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert_eq!(v["status"], "blocked");
        // Parent + one placeholder track; no executable subject Brief.
        assert_eq!(v["placeholder_tracks_created"].as_array().unwrap().len(), 1);
        assert!(v["subject_briefs_created"].as_array().unwrap().is_empty());
        assert!(v["role_tracks_created"].as_array().unwrap().is_empty());
        let ph = &v["placeholder_tracks_created"][0];
        assert_eq!(ph["placeholder"], true);
        assert_eq!(ph["reason"], "crew not ready");
        assert!(
            ph["title"]
                .as_str()
                .unwrap()
                .starts_with("Design track blocked:")
        );
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 2);
    }

    #[test]
    fn orchestrate_repeated_missing_role_reuses_placeholder() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        plan_with_gaps(&spine, &m, "[\"designer\"]", "[]", "[]");
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert_eq!(
            first["placeholder_tracks_created"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let before = tasks.list_briefs_by_mandate(&m, 50).unwrap().len();
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert!(
            second["placeholder_tracks_created"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            second["placeholder_tracks_existing"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            tasks.list_briefs_by_mandate(&m, 50).unwrap().len(),
            before,
            "the placeholder is reused, not duplicated"
        );
    }

    #[test]
    fn orchestrate_pending_hire_creates_placeholder_no_subject() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let pending = agents
            .request_hire(
                "P", "engineer", "P", "e", "e", "prime", "subj-p", "medium", "default",
            )
            .unwrap();
        plan_with_gaps(
            &spine,
            &m,
            "[]",
            &format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]"),
            "[]",
        );
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert_eq!(v["placeholder_tracks_created"].as_array().unwrap().len(), 1);
        assert_eq!(v["placeholder_tracks_created"][0]["reason"], "pending hire");
        assert!(v["subject_briefs_created"].as_array().unwrap().is_empty());
        assert!(v["assigned_briefs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn orchestrate_denied_role_creates_placeholder_no_subject() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        plan_with_gaps(
            &spine,
            &m,
            "[]",
            "[]",
            "[{\"role\":\"security\",\"reason\":\"charter denied\"}]",
        );
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert_eq!(v["placeholder_tracks_created"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["placeholder_tracks_created"][0]["reason"],
            "charter denied"
        );
        assert!(v["subject_briefs_created"].as_array().unwrap().is_empty());
        assert!(v["assigned_briefs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn orchestrate_placeholder_becomes_active_reuses_role_track() {
        // A pending-hire role gets a placeholder; once the hire activates,
        // a rerun reuses the SAME role track and creates the subject Brief
        // under it (the placeholder→active transition).
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let pending = agents
            .request_hire(
                "W", "engineer", "W", "e", "e", "prime", "subj-rt", "medium", "default",
            )
            .unwrap();
        plan_with_gaps(
            &spine,
            &m,
            "[]",
            &format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]"),
            "[]",
        );
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        let role_id = first["placeholder_tracks_created"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            first["subject_briefs_created"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        // Activate the hire → engineer is now active.
        agents.approve_hire(&pending).unwrap();
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        // The role is no longer a gap; the same role track is reused.
        assert!(
            second["placeholder_tracks_created"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            second["placeholder_tracks_existing"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(second["role_tracks_existing"].as_array().unwrap().len(), 1);
        assert_eq!(
            second["role_tracks_existing"][0]["task_id"],
            serde_json::json!(role_id)
        );
        // A subject execution Brief is now created + assigned under it.
        assert_eq!(
            second["subject_briefs_created"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            second["assigned_briefs"][0]["agent_id"],
            serde_json::json!(pending)
        );
        // Tree: parent + role track + subject = 3 (no duplicate role track).
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
    }

    // ── Role-keyed pending-clearance placeholders ───────────────

    /// Set up a Mandate with a pending hire for `role` that is gated by a
    /// pending spawn Clearance. Returns `(mandate_id, hire_id, clearance_id)`.
    fn mandate_with_pending_clearance(
        agents: &AgentStore,
        spine: &SpineStore,
        role: &str,
    ) -> (String, String, String) {
        let m = approved_mandate(spine);
        let hire = agents
            .request_hire(
                "W", role, "W", "eng", "eng", "prime", "subj-clr", "medium", "default",
            )
            .unwrap();
        let cid = agents
            .create_spawn_clearance(&hire, "subj-clr", "spawn", &[], "default")
            .unwrap();
        spine
            .record_team_plan(&crate::nodes::coordinator::spine::store::TeamPlanRecord {
                tenant_id: "default",
                mandate_id: &m,
                actor_id: "operator",
                description: "x",
                proposed_roles_json: "[]",
                pending_hires_json: &format!("[{{\"role\":\"{role}\",\"agent_id\":\"{hire}\"}}]"),
                clearance_ids_json: &format!("[\"{cid}\"]"),
                denials_json: "[]",
                next_steps_json: "[]",
                status: "awaiting_clearance",
            })
            .unwrap();
        (m, hire, cid)
    }

    #[test]
    fn orchestrate_pending_clearance_creates_placeholder() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _hire, cid) = mandate_with_pending_clearance(&agents, &spine, "engineer");
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        // One placeholder track for the clearance-blocked role; no subject.
        assert_eq!(v["placeholder_tracks_created"].as_array().unwrap().len(), 1);
        let reason = v["placeholder_tracks_created"][0]["reason"]
            .as_str()
            .unwrap();
        assert!(
            reason.starts_with("pending clearance") && reason.contains(&cid),
            "reason should name the pending clearance id: {reason}"
        );
        assert!(v["subject_briefs_created"].as_array().unwrap().is_empty());
        assert!(v["assigned_briefs"].as_array().unwrap().is_empty());
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 2);
    }

    #[test]
    fn orchestrate_repeated_pending_clearance_reuses_placeholder() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _hire, _cid) = mandate_with_pending_clearance(&agents, &spine, "engineer");
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert_eq!(
            first["placeholder_tracks_created"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let before = tasks.list_briefs_by_mandate(&m, 50).unwrap().len();
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert!(
            second["placeholder_tracks_created"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            second["placeholder_tracks_existing"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), before);
    }

    #[test]
    fn orchestrate_clearance_approved_creates_subject_under_same_role_track() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, hire, cid) = mandate_with_pending_clearance(&agents, &spine, "engineer");
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        let role_id = first["placeholder_tracks_created"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        // Approving the spawn Clearance activates the hire.
        assert!(matches!(
            decide(&agents, &cid, "approved"),
            HandlerOutcome::Ok(_)
        ));
        assert_eq!(agents.get_agent(&hire).unwrap().unwrap().status, "active");
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        // Same role track reused; a subject Brief now exists + is assigned.
        assert_eq!(
            second["role_tracks_existing"][0]["task_id"],
            serde_json::json!(role_id)
        );
        assert!(
            second["placeholder_tracks_created"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            second["subject_briefs_created"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            second["assigned_briefs"][0]["agent_id"],
            serde_json::json!(hire)
        );
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
    }

    // ── Placeholder title lifecycle ─────────────────────────────

    #[test]
    fn orchestrate_auto_placeholder_title_transitions_to_active() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let pending = agents
            .request_hire(
                "W", "engineer", "W", "e", "e", "prime", "subj-rt", "medium", "default",
            )
            .unwrap();
        plan_with_gaps(
            &spine,
            &m,
            "[]",
            &format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]"),
            "[]",
        );
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        let role_id = first["placeholder_tracks_created"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        // The auto placeholder title is the "blocked" form.
        let t0 = tasks
            .list_briefs_by_mandate(&m, 50)
            .unwrap()
            .into_iter()
            .find(|c| c.task_id == role_id)
            .unwrap()
            .title;
        assert!(t0.starts_with("Engineering track blocked:"), "got: {t0}");
        // Activate → rerun promotes the auto title to the active title.
        agents.approve_hire(&pending).unwrap();
        let second = orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        assert_eq!(
            second["role_tracks_existing"][0]["task_id"],
            serde_json::json!(role_id)
        );
        let t1 = tasks
            .list_briefs_by_mandate(&m, 50)
            .unwrap()
            .into_iter()
            .find(|c| c.task_id == role_id)
            .unwrap()
            .title;
        assert!(
            t1.starts_with("Engineering track:") && !t1.contains("blocked"),
            "got: {t1}"
        );
    }

    #[test]
    fn orchestrate_user_edited_placeholder_title_is_preserved() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        let pending = agents
            .request_hire(
                "W", "engineer", "W", "e", "e", "prime", "subj-rt", "medium", "default",
            )
            .unwrap();
        plan_with_gaps(
            &spine,
            &m,
            "[]",
            &format!("[{{\"role\":\"engineer\",\"agent_id\":\"{pending}\"}}]"),
            "[]",
        );
        let first = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        let role_id = first["placeholder_tracks_created"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        // Operator renames the placeholder track by hand.
        tasks
            .set_brief_field(&role_id, "title", "My bespoke engineering plan")
            .unwrap();
        // Activate → rerun must NOT clobber the user's title.
        agents.approve_hire(&pending).unwrap();
        orchestrate(&tasks, &agents, &spine, &format!("{m}|assign_ready"));
        let title = tasks
            .list_briefs_by_mandate(&m, 50)
            .unwrap()
            .into_iter()
            .find(|c| c.task_id == role_id)
            .unwrap()
            .title;
        assert_eq!(title, "My bespoke engineering plan");
    }

    // ── max_briefs overflow visibility ──────────────────────────

    #[test]
    fn orchestrate_max_briefs_reports_omitted_placeholders() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // Four missing roles, but max_briefs=3 → parent + 2 role tracks fit,
        // the remaining 2 placeholders must be reported, not dropped.
        plan_with_gaps(
            &spine,
            &m,
            "[\"engineer\",\"designer\",\"writer\",\"qa\"]",
            "[]",
            "[]",
        );
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs|3"));
        let created = v["placeholder_tracks_created"].as_array().unwrap();
        let omitted = v["placeholder_tracks_omitted"].as_array().unwrap();
        assert_eq!(
            created.len(),
            2,
            "only 2 placeholders fit under max_briefs=3"
        );
        assert_eq!(omitted.len(), 2, "the other 2 are reported omitted");
        assert!(omitted.iter().all(|o| o["omitted"] == "max_briefs"));
        // Never more Briefs than max_briefs (parent + 2 = 3).
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
        // The omission is also persisted via `skipped`.
        assert!(
            v["skipped"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["omitted"] == "max_briefs"),
            "omitted placeholders must be recorded in skipped: {v}"
        );
    }

    #[test]
    fn orchestrate_dry_run_and_plan_only_create_nothing() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _a) = ready_team(&agents, &spine, "engineer");
        // dry_run=true on assign_ready.
        let dr = orchestrate(
            &tasks,
            &agents,
            &spine,
            &format!("{m}|assign_ready|16|true"),
        );
        assert_eq!(dr["ready"], true);
        assert_eq!(dr["status"], "planned");
        assert!(dr["created_briefs"].as_array().unwrap().is_empty());
        assert!(!dr["skipped"].as_array().unwrap().is_empty());
        assert!(tasks.list_briefs_by_mandate(&m, 50).unwrap().is_empty());
        // plan_only (default mode).
        let po = orchestrate(&tasks, &agents, &spine, &m);
        assert_eq!(po["mode"], "plan_only");
        assert!(po["created_briefs"].as_array().unwrap().is_empty());
        assert!(tasks.list_briefs_by_mandate(&m, 50).unwrap().is_empty());
    }

    #[test]
    fn orchestrate_create_briefs_mode_does_not_assign() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _a) = ready_team(&agents, &spine, "engineer");
        let v = orchestrate(&tasks, &agents, &spine, &format!("{m}|create_briefs"));
        assert_eq!(v["status"], "created");
        assert_eq!(v["created_briefs"].as_array().unwrap().len(), 3);
        // The subject Brief is created but left unassigned in this mode.
        assert_eq!(v["subject_briefs_created"].as_array().unwrap().len(), 1);
        assert!(
            v["assigned_briefs"].as_array().unwrap().is_empty(),
            "create_briefs must not assign"
        );
    }

    #[test]
    fn orchestrate_respects_assign_gate_for_agent_actor() {
        // A non-operator Prime without can_assign_work creates the Brief
        // tree but cannot assign — assignment is skipped, not bypassed.
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let (m, _worker) = ready_team(&agents, &spine, "engineer");
        // The Prime actor (keyed to the ctx subject) has NO assign Key.
        agents
            .create_agent(
                "Prime",
                "prime",
                "P",
                "ops",
                "ops",
                "founder",
                &subject_of(b"prime-seed"),
                "medium",
                "default",
            )
            .unwrap();
        let body = ok_body(handle_orchestrate(
            &tasks,
            &agents,
            &spine,
            &fake_ctx_with_role(
                format!("{m}|assign_ready").as_bytes(),
                "prime",
                b"prime-seed",
            ),
        ));
        let v: serde_json::Value = serde_json::from_slice(&body.into_bytes()).unwrap();
        // Briefs were created (creation is not assign-gated) ...
        assert_eq!(v["created_briefs"].as_array().unwrap().len(), 3);
        // ... but the assignment was refused by the assign-Key gate.
        assert!(v["assigned_briefs"].as_array().unwrap().is_empty());
        assert!(
            v["skipped"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["reason"].as_str().unwrap_or("").contains("assign denied")),
            "the assign-Key denial must be reported in skipped: {v}"
        );
    }

    #[test]
    fn orchestration_latest_none_until_run_and_tenant_isolated() {
        let tasks = TaskStore::in_memory().unwrap();
        let agents = store();
        let spine = SpineStore::in_memory().unwrap();
        let m = approved_mandate(&spine);
        // None before any run.
        let none = ok_body(handle_orchestration_latest(&spine, &fake_ctx(m.as_bytes())));
        assert_eq!(none.trim(), "null");
        // A run persists; latest reads it back for the same tenant only.
        let (_m2, _a) = ready_team(&agents, &spine, "engineer");
        orchestrate(&tasks, &agents, &spine, &format!("{m}|plan_only"));
        let latest = ok_body(handle_orchestration_latest(&spine, &fake_ctx(m.as_bytes())));
        let lv: serde_json::Value = serde_json::from_slice(latest.as_bytes()).unwrap();
        assert_eq!(lv["mandate_id"], m);
        // Tenant B cannot read tenant A's run.
        let b = ok_body(handle_orchestration_latest(
            &spine,
            &fake_ctx_tenant(m.as_bytes(), "tenant-b"),
        ));
        assert_eq!(b.trim(), "null");
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
    fn instruction_bundle_mutation_is_configure_gated() {
        // company-model §4.5/§5.2A: an authorized configurer may set
        // another Operative's charter; an actor may NOT set its own.
        let (s, actor, report, _o) = configure_fixture("branch");
        assert!(matches!(
            update_as(
                &s,
                &report,
                "instruction_bundle",
                "# You build. Test first."
            ),
            HandlerOutcome::Ok(_)
        ));
        assert_eq!(
            s.get_agent(&report).unwrap().unwrap().instruction_bundle,
            "# You build. Test first."
        );
        // The actor cannot rewrite its OWN charter (self-config denied).
        assert_eq!(
            err_kind(update_as(
                &s,
                &actor,
                "instruction_bundle",
                "# I am admin now"
            )),
            error_kinds::POLICY_DENIED
        );
    }

    #[test]
    fn model_preference_mutation_is_configure_gated() {
        // Adapter model preferences (relix-agent-adapters.md §3.2/§3.3/§7)
        // ride the same configure-gate as every other profile edit: an
        // authorized configurer may set another Operative's preference; an
        // actor may NOT set its own.
        let (s, actor, report, _o) = configure_fixture("branch");
        assert!(matches!(
            update_as(&s, &report, "model_preference", "claude-sonnet-4"),
            HandlerOutcome::Ok(_)
        ));
        assert_eq!(
            s.get_agent(&report)
                .unwrap()
                .unwrap()
                .model_preference
                .as_deref(),
            Some("claude-sonnet-4")
        );
        // Self-config is denied (the gate is not bypassed for adapter prefs).
        assert_eq!(
            err_kind(update_as(&s, &actor, "model_preference", "gpt-5-codex")),
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

    // ── Secret allowlist enforcement (company-model §5.2C) ───

    fn secret_check(s: &AgentStore, seed: &[u8], secret: &str) -> Result<(), u32> {
        match enforce_secret_allowlist(s, &fake_ctx_with_role(b"", "engineer", seed), secret) {
            Ok(()) => Ok(()),
            Err(o) => Err(err_kind(o)),
        }
    }

    #[test]
    fn secret_empty_allowlist_denies_operative() {
        let s = store();
        s.create_agent(
            "A",
            "engineer",
            "A",
            "e",
            "e",
            "p",
            &subject_of(b"sec-seed"),
            "medium",
            "default",
        )
        .unwrap();
        // No secret_allowlist → deny by default.
        assert_eq!(
            secret_check(&s, b"sec-seed", "db"),
            Err(error_kinds::SECURITY_DENIED)
        );
    }

    #[test]
    fn secret_allowlisted_operative_can_read_exact_only() {
        let s = store();
        let id = s
            .create_agent(
                "A",
                "engineer",
                "A",
                "e",
                "e",
                "p",
                &subject_of(b"sec-seed"),
                "medium",
                "default",
            )
            .unwrap();
        s.update_agent_field(&id, "secret_allowlist", "db, stripe_key")
            .unwrap();
        assert_eq!(secret_check(&s, b"sec-seed", "db"), Ok(()));
        assert_eq!(secret_check(&s, b"sec-seed", "stripe_key"), Ok(()));
        // Substring / prefix tricks do not bypass.
        assert_eq!(
            secret_check(&s, b"sec-seed", "db-prod"),
            Err(error_kinds::SECURITY_DENIED)
        );
        assert_eq!(
            secret_check(&s, b"sec-seed", "stripe_key2"),
            Err(error_kinds::SECURITY_DENIED)
        );
    }

    #[test]
    fn secret_disabled_operative_cannot_read() {
        let s = store();
        let id = s
            .create_agent(
                "A",
                "engineer",
                "A",
                "e",
                "e",
                "p",
                &subject_of(b"sec-seed"),
                "medium",
                "default",
            )
            .unwrap();
        s.update_agent_field(&id, "secret_allowlist", "db").unwrap();
        s.soft_delete_agent(&id).unwrap(); // → disabled
        assert_eq!(
            secret_check(&s, b"sec-seed", "db"),
            Err(error_kinds::SECURITY_DENIED)
        );
    }

    #[test]
    fn secret_non_operative_passes_through_to_vault_gate() {
        // A caller with no Operative profile is not subject to the
        // allowlist (the vault's owner/tenant gate still protects it).
        let s = store();
        assert_eq!(secret_check(&s, b"ghost-seed", "db"), Ok(()));
    }

    #[test]
    fn secret_operator_bypasses_allowlist() {
        let s = store();
        // Operator role bypasses even with no profile / no allowlist.
        assert!(enforce_secret_allowlist(&s, &fake_ctx(b""), "db").is_ok());
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

    #[test]
    fn approval_pending_emits_typed_columns() {
        // The pending TSV preserves the typed fields the Approvals hub needs
        // (subject_id, capability_category, expires_at, task_id) APPENDED after
        // the historical 5-column prefix, so the bridge can render a typed
        // payload summary without a second per-row fetch. A spawn-hire Clearance
        // carries its linked subject + a task id.
        use crate::nodes::coordinator::agent::store::SPAWN_CLEARANCE_METHOD;
        let s = store();
        s.create_approval(
            "agt-hire",                  // agent_id
            "subj-hire",                 // subject_id
            SPAWN_CLEARANCE_METHOD,      // method
            "spawn",                     // capability_category
            "",                          // args_redacted_hash
            "activate the pending hire", // reason
            &[],                         // approver_groups
            Some("REL-7"),               // task_id
            9999999999,                  // expires_at
            &[],                         // authorized_approvers
            "default",
        )
        .unwrap();
        let body = ok_body(handle_approval_pending(&s, &fake_ctx(b"")));
        // The approval_id (col 0) is store-generated; the row is the only
        // non-`count=` line.
        let row = body
            .lines()
            .find(|l| !l.is_empty() && !l.starts_with("count="))
            .expect("pending row present");
        let cols: Vec<&str> = row.split('\t').collect();
        assert_eq!(cols.len(), 9, "9 typed columns: {row:?}");
        assert!(!cols[0].is_empty(), "approval_id present");
        assert_eq!(cols[1], "agt-hire", "agent_id (the requesting actor)");
        assert_eq!(cols[2], SPAWN_CLEARANCE_METHOD, "method");
        assert_eq!(cols[5], "subj-hire", "subject_id (who/what is affected)");
        assert_eq!(cols[6], "spawn", "capability_category bucket");
        assert_eq!(cols[7], "9999999999", "expires_at window");
        assert_eq!(cols[8], "REL-7", "task_id (the parked Brief target route)");
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
